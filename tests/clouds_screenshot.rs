//! R6.0 (C0) headless screenshot smoke test: renders the clouds plugin's
//! DEBUG SHELL through the whole chain — march → temporal resolve → composite
//! — and asserts the frame is a uniform, warm, jittered shell of the expected
//! luminance.
//!
//! The debug shell (mode 1) paints every ray that enters the cloud layer with
//! a per-texel hash jitter (±25%). That is exactly what makes this a good
//! smoke test for the C0 pipeline: a flat field would hide a broken resolve,
//! while the jitter survives it and its statistics are computable. The real
//! density model replaces the shell in C1.
//!
//! Requires a real GPU (atmosphere/cloud compute pipelines). The test is
//! `#[ignore]`d: run locally with
//! `cargo test --test clouds_screenshot -- --ignored --nocapture`.

use bevy::asset::AssetPlugin;
use bevy::camera::CameraPlugin;
use bevy::camera::Exposure;
use bevy::camera::ImageRenderTarget;
use bevy::camera::RenderTarget;
use bevy::core_pipeline::CorePipelinePlugin;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::image::ImagePlugin;
use bevy::log::LogPlugin;
use bevy::mesh::MeshPlugin;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use bevy::render::RenderPlugin;
use bevy::render::render_resource::TextureFormat;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured};
use bevy_elements::ElementsCamera;

use no_one_looks_up::plugins::GamePlugins;
use no_one_looks_up::plugins::clouds::CloudsDebug;

use std::time::Duration;

#[derive(Resource, Default)]
struct FrameCounter(usize);

#[derive(Resource, Default)]
struct CapturedFrame(Option<Image>);

#[test]
#[ignore = "requires a physical GPU (atmosphere/cloud compute); run locally with --ignored"]
fn debug_shell_renders_through_the_chain() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        bevy::transform::TransformPlugin,
        AssetPlugin::default(),
        RenderPlugin::default(),
        WindowPlugin {
            primary_window: None,
            exit_condition: bevy::window::ExitCondition::DontExit,
            close_when_requested: false,
            primary_cursor_options: None,
        },
        ImagePlugin::default(),
        MeshPlugin,
        CameraPlugin,
        CorePipelinePlugin,
        bevy::input::InputPlugin,
    ));
    app.add_plugins((
        bevy::post_process::PostProcessPlugin,
        bevy::gltf::GltfPlugin::default(),
        bevy::pbr::PbrPlugin::default(),
        bevy::light::LightPlugin,
        bevy::gizmos::GizmoPlugin,
        GamePlugins,
        LogPlugin::default(),
    ));
    app.finish();
    app.init_resource::<FrameCounter>();
    app.init_resource::<CapturedFrame>();
    // The debug shell: every ray into the layer shows it (the C0 chain check).
    // Luminance is in the sky's unexposed radiance units: the composite
    // multiplies by `view.exposure` (≈1e-4 at ev100 13), so ~6000 lands mid
    // grey after tonemapping.
    app.insert_resource(CloudsDebug::shell(6000.0));

    let target = {
        let mut images = app.world_mut().resource_mut::<Assets<Image>>();
        images.add(Image::new_target_texture(
            320,
            180,
            TextureFormat::Rgba16Float,
            None,
        ))
    };

    app.world_mut().spawn((
        Name::new("ScreenshotCamera"),
        Camera3d::default(),
        RenderTarget::Image(ImageRenderTarget {
            handle: target,
            scale_factor: 1.0,
        }),
        ElementsCamera {
            auto_exposure: false,
            ..ElementsCamera::default()
        },
        Exposure { ev100: 13.0 },
        // The frame statistics assert pre-tonemap ratios (ACES compresses the
        // 1.67× jitter to ~1.19×); tonemapping is part of the visual check a
        // human does on the saved PNG, not of this test.
        Tonemapping::None,
        Bloom::NATURAL,
        // Pitched 45° up: inside the game's 22.55–60° limits, and every ray
        // meets the layer.
        Transform::from_xyz(0.0, 1.6, 0.0).with_rotation(Quat::from_euler(
            EulerRot::YXZ,
            0.0,
            45f32.to_radians(),
            0.0,
        )),
    ));

    app.add_systems(
        Update,
        (bump_frame_counter, take_screenshot.run_if(in_frame(150))).chain(),
    );

    let mut frames = 0usize;
    loop {
        app.update();
        frames += 1;
        std::thread::sleep(Duration::from_millis(16));
        let captured = app
            .world()
            .get_resource::<CapturedFrame>()
            .and_then(|c| c.0.as_ref());
        if let Some(image) = captured {
            assert_frame(image);
            return;
        }
        assert!(frames < 600, "screenshot not captured within 600 frames");
    }
}

fn bump_frame_counter(mut counter: ResMut<FrameCounter>) {
    counter.0 += 1;
}

fn in_frame(frame: usize) -> impl FnMut(Res<FrameCounter>) -> bool + Clone {
    move |counter: Res<FrameCounter>| counter.0 == frame
}

/// Requests the screenshot capture (entity despawns after capture).
fn take_screenshot(mut commands: Commands, targets: Query<&RenderTarget, With<ElementsCamera>>) {
    let Ok(target) = targets.single() else {
        return;
    };
    let RenderTarget::Image(image_target) = target else {
        return;
    };
    let handle = image_target.handle.clone();
    let entity = commands
        .spawn(Screenshot::image(handle.clone()))
        .observe(
            |trigger: On<ScreenshotCaptured>, mut frame: ResMut<CapturedFrame>| {
                frame.0 = Some(trigger.image.clone());
            },
        )
        .id();
    commands
        .entity(entity)
        .observe(bevy::render::view::screenshot::save_to_disk(
            "target/clouds_screenshot.png",
        ));
}

/// Asserts the captured linear HDR frame is the debug shell.
///
/// The camera is pitched 45° up, so every ray meets the layer and the composite
/// (blend `src.rgb + dst.rgb · src.a` with transmittance 0) replaces the whole
/// frame with the shell: warm white at luminance 0.5 nits with ±25% jitter,
/// times the exposure. Assertions are ratio-based, so the exact exposure does
/// not matter.
fn assert_frame(image: &Image) {
    let planes = f32_plane(image);
    assert_eq!(planes.len() % 4, 0, "expected RGBA data");
    let pixels = planes.len() / 4;
    assert!(pixels > 0, "empty frame");

    let mut lumas = Vec::with_capacity(pixels);
    let mut bad = 0usize;
    for px in planes.as_chunks::<4>().0 {
        if px[0].is_nan() || px[1].is_nan() || px[2].is_nan() || px[3].is_nan() {
            bad += 1;
            continue;
        }
        lumas.push(0.2126 * px[0] + 0.7152 * px[1] + 0.0722 * px[2]);
    }
    assert_eq!(bad, 0, "{bad} NaN pixels in the captured frame");
    assert!(!lumas.is_empty(), "frame is empty");

    lumas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = lumas.iter().sum::<f32>() / lumas.len() as f32;
    let max = lumas[lumas.len() - 1];
    let p90 = lumas[(lumas.len() as f32 * 0.9) as usize];
    let p10 = lumas[(lumas.len() as f32 * 0.1) as usize];

    // Mean colour of the brightest band (the shell dominates everywhere).
    let mut warm = [0.0f64; 3];
    for px in planes.as_chunks::<4>().0 {
        for c in 0..3 {
            warm[c] += px[c] as f64;
        }
    }
    let warm = [
        (warm[0] / pixels as f64) / mean as f64,
        (warm[1] / pixels as f64) / mean as f64,
        (warm[2] / pixels as f64) / mean as f64,
    ];

    println!(
        "clouds debug-shell stats: mean={mean:.3} max={max:.3} p10={p10:.3} p90={p90:.3} \
         relative rgb=({:.3}, {:.3}, {:.3})",
        warm[0], warm[1], warm[2]
    );

    assert!(
        mean > 0.1,
        "frame is dark (mean luma {mean:.4}); the debug shell did not render \
         — check the kernel compile log for WESL import errors"
    );
    assert!(
        warm[0] > warm[2] * 1.15,
        "shell is not warm (relative R {:.3} vs B {:.3}); the shell colour changed",
        warm[0],
        warm[2]
    );
    // Uniform luminance with ±25% jitter: p90/p10 ≈ 1.6. A sky frame (no
    // shell) would show a far wider spread; a resolve that smeared the jitter
    // to nothing would collapse to 1.0.
    assert!(
        p90 > p10 * 1.2,
        "frame is too uniform (p10 {p10:.3} vs p90 {p90:.3}); the per-texel \
         jitter was lost — march or resolve is broken"
    );
    // And not so noisy that the temporal chain is amplifying garbage.
    assert!(
        p90 < p10 * 2.5,
        "frame is too noisy (p10 {p10:.3} vs p90 {p90:.3}); the resolve is \
         diverging instead of converging"
    );
}

/// Converts the RGBA16Float texture data to normalized f32 values.
fn f32_plane(image: &Image) -> Vec<f32> {
    assert_eq!(image.texture_descriptor.format, TextureFormat::Rgba16Float);
    let data = image.data.as_ref().expect("CPU-visible image data");
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|b| f16_to_f32(u16::from_le_bytes([b[0], b[1]])))
        .collect()
}

/// Minimal IEEE 754 half -> f32 conversion (no `half` dependency needed).
fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = (h & 0x7C00) >> 10;
    let frac = (h & 0x03FF) as u32;
    let bits = match exp {
        0 => {
            if frac == 0 {
                sign
            } else {
                // Subnormal: normalize.
                let mut e: u32 = 127 - 15 + 1;
                let mut f = frac;
                while f & 0x0400 == 0 {
                    f <<= 1;
                    e -= 1;
                }
                f &= 0x03FF;
                sign | ((e + 9) << 23) | (f << 13)
            }
        }
        0x1F => sign | 0x7F80_0000 | (frac << 13),
        _ => sign | (((exp as u32) + 112) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}
