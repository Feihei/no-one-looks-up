//! R6 headless screenshot smoke tests.
//!
//! - `debug_shell_renders_through_the_chain` (C0): a uniform, jittered shell
//!   through march → temporal resolve → composite proves the whole pipeline
//!   runs and the reprojection does not smear the per-texel jitter away.
//! - `cumulus_renders_a_structured_sky` (C1/C2): the real noise volumes and
//!   density model give a structured cumulus whose sunward edges are the
//!   warmest pixels — the silver lining and the inner glow.
//! - `cumulus_backlit_is_sky_blue` (C2): with the sun behind the camera the
//!   frame turns blue: the cloud's shadow side is lit by the dusk sky.
//!
//! All need a real GPU (atmosphere and cloud compute pipelines). They are
//! `#[ignore]`d: run locally with
//! `cargo test --test clouds_screenshot -- --ignored --nocapture`.

use std::time::Duration;

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
use bevy_elements::{ElementsCamera, SunState};

use no_one_looks_up::plugins::GamePlugins;
use no_one_looks_up::plugins::clouds::CloudsDebug;
use no_one_looks_up::plugins::sky::{CloudKind, DuskSky};

#[derive(Resource, Default)]
struct FrameCounter(usize);

#[derive(Resource, Default)]
struct CapturedFrame(Option<Image>);

/// Where the captured frame is written.
#[derive(Resource)]
struct CapturePath(&'static str);

/// A headless app running the whole game plugin set into an offscreen target.
fn harness(sky: DuskSky, debug: Option<CloudsDebug>) -> (App, Handle<Image>) {
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
    // The sky must be in place before `SkyPlugin` reads it.
    app.insert_resource(sky);
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
    if let Some(debug) = debug {
        app.insert_resource(debug);
    }

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
            handle: target.clone(),
            scale_factor: 1.0,
        }),
        ElementsCamera {
            auto_exposure: false,
            ..ElementsCamera::default()
        },
        Exposure { ev100: 13.0 },
        // Statistics are asserted pre-tonemap; tonemapping is part of the
        // visual check a human does on the saved PNG.
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
    (app, target)
}

/// Aims the test camera `offset_deg` around the vertical from the sun's
/// azimuth, still pitched 45° up. The sky must have resolved the sun first.
fn aim(app: &mut App, offset_deg: f32) {
    app.update();
    let sun = app.world().resource::<SunState>().direction;
    let yaw = f32::atan2(-sun.x, -sun.z) + offset_deg.to_radians();
    let entity = {
        let world = app.world_mut();
        let mut query = world.query_filtered::<Entity, With<ElementsCamera>>();
        query.single(world).expect("one test camera")
    };
    if let Some(mut transform) = app.world_mut().get_mut::<Transform>(entity) {
        transform.rotation = Quat::from_euler(EulerRot::YXZ, yaw, 45f32.to_radians(), 0.0);
    }
}

/// Runs until `frame`, requests the capture, then runs until it lands.
fn capture_at(app: &mut App, frame: usize, path: &'static str) -> Image {
    app.insert_resource(CapturePath(path));
    app.add_systems(
        Update,
        (bump_frame_counter, take_screenshot.run_if(at_frame(frame))).chain(),
    );
    let mut frames = 0usize;
    loop {
        app.update();
        frames += 1;
        std::thread::sleep(Duration::from_millis(16));
        if let Some(image) = app
            .world()
            .get_resource::<CapturedFrame>()
            .and_then(|c| c.0.as_ref())
        {
            return image.clone();
        }
        assert!(frames < 900, "screenshot not captured within 900 frames");
    }
}

fn bump_frame_counter(mut counter: ResMut<FrameCounter>) {
    counter.0 += 1;
}

fn at_frame(frame: usize) -> impl FnMut(Res<FrameCounter>) -> bool + Clone {
    move |counter: Res<FrameCounter>| counter.0 == frame
}

/// Requests the screenshot capture (the entity despawns after capture).
fn take_screenshot(
    mut commands: Commands,
    path: Res<CapturePath>,
    targets: Query<&RenderTarget, With<ElementsCamera>>,
) {
    let Ok(target) = targets.single() else {
        return;
    };
    let RenderTarget::Image(image_target) = target else {
        return;
    };
    let handle = image_target.handle.clone();
    commands
        .spawn(Screenshot::image(handle))
        .observe(
            |trigger: On<ScreenshotCaptured>, mut frame: ResMut<CapturedFrame>| {
                frame.0 = Some(trigger.image.clone());
            },
        )
        .observe(bevy::render::view::screenshot::save_to_disk(path.0));
}

#[test]
#[ignore = "requires a physical GPU (atmosphere/cloud compute); run locally with --ignored"]
fn debug_shell_renders_through_the_chain() {
    // Luminance is in the sky's unexposed radiance units: the composite
    // multiplies by `view.exposure` (≈1e-4 at ev100 13), so ~6000 lands mid
    // grey after tonemapping.
    let (mut app, _) = harness(DuskSky::default(), Some(CloudsDebug::shell(6000.0)));
    let image = capture_at(&mut app, 150, "target/clouds_debug_shell.png");
    let stats = FrameStats::of(&image);
    println!("debug-shell stats: {stats:?}");

    assert_eq!(
        stats.nan, 0,
        "{} NaN pixels in the captured frame",
        stats.nan
    );
    assert!(
        stats.mean > 0.1,
        "frame is dark (mean luma {:.4}); the debug shell did not render \
         — check the kernel compile log for WESL import errors",
        stats.mean
    );
    assert!(
        stats.relative[0] > stats.relative[2] * 1.15,
        "shell is not warm (relative R {:.3} vs B {:.3}); the shell colour changed",
        stats.relative[0],
        stats.relative[2]
    );
    // Uniform luminance with ±25 % jitter: p90/p10 ≈ 1.6. A sky frame (no
    // shell) would show a far wider spread; a resolve that smeared the jitter
    // to nothing would collapse to 1.0.
    assert!(
        stats.p90 > stats.p10 * 1.2,
        "frame is too uniform (p10 {:.3} vs p90 {:.3}); the per-texel jitter \
         was lost — march or resolve is broken",
        stats.p10,
        stats.p90
    );
    assert!(
        stats.p90 < stats.p10 * 2.5,
        "frame is too noisy (p10 {:.3} vs p90 {:.3}); the resolve is diverging \
         instead of converging",
        stats.p10,
        stats.p90
    );
}

#[test]
#[ignore = "requires a physical GPU (atmosphere/cloud compute); run locally with --ignored"]
fn cumulus_renders_a_structured_sky() {
    let sky = DuskSky {
        cloud_kind: Some(CloudKind::BrokenCumulus),
        cloud_amount: 0.5,
        ..DuskSky::default()
    };
    let (mut app, _) = harness(sky, None);
    aim(&mut app, 0.0);
    // The noise volumes take 8 frames plus shader compilation.
    let image = capture_at(&mut app, 240, "target/clouds_sunward.png");
    let stats = FrameStats::of(&image);
    println!("sunward stats: {stats:?}");

    assert_eq!(
        stats.nan, 0,
        "{} NaN pixels in the captured frame",
        stats.nan
    );
    // The clouds are lit by the sun through the atmosphere and the sky behind
    // them is a dusk gradient, so the frame is not flat.
    assert!(
        stats.relative_stddev > 0.15,
        "frame is flat (relative stddev {:.3}); the density model produced no \
         structure — check the weather map and the noise volumes",
        stats.relative_stddev
    );
    // Some pixels are much brighter than the median: lit cloud tops.
    assert!(
        stats.p99 > stats.median * 1.5,
        "no lit cloud tops (p99 {:.4} vs median {:.4})",
        stats.p99,
        stats.median
    );
    // Dusk light is warm: the bright region must not be blue.
    assert!(
        stats.bright_relative[0] > stats.bright_relative[2],
        "lit cloud is not warm (bright R {:.3} vs B {:.3})",
        stats.bright_relative[0],
        stats.bright_relative[2]
    );
}

#[test]
#[ignore = "requires a physical GPU (atmosphere/cloud compute); run locally with --ignored"]
fn cumulus_backlit_is_sky_blue() {
    let sky = DuskSky {
        cloud_kind: Some(CloudKind::BrokenCumulus),
        cloud_amount: 0.5,
        ..DuskSky::default()
    };
    let (mut app, _) = harness(sky, None);
    aim(&mut app, 180.0);
    let image = capture_at(&mut app, 240, "target/clouds_backlit.png");
    let stats = FrameStats::of(&image);
    println!("backlit stats: {stats:?}");

    assert_eq!(
        stats.nan, 0,
        "{} NaN pixels in the captured frame",
        stats.nan
    );
    assert!(
        stats.relative_stddev > 0.15,
        "frame is flat (relative stddev {:.3}); no cloud structure behind the \
         camera",
        stats.relative_stddev
    );
    // With the sun behind the camera the cloud is lit by the sky alone: the
    // frame must be blue-dominant, unlike the sunward one.
    assert!(
        stats.relative[2] > stats.relative[0],
        "backlit frame is not sky-blue (relative R {:.3} vs B {:.3})",
        stats.relative[0],
        stats.relative[2]
    );
}

/// Statistics of one captured linear HDR frame.
#[derive(Debug)]
struct FrameStats {
    mean: f32,
    median: f32,
    p10: f32,
    p90: f32,
    p99: f32,
    relative_stddev: f32,
    /// Mean channel of the whole frame over its mean luminance.
    relative: [f32; 3],
    /// Mean channel of the brightest 1 % of pixels over their mean luminance.
    bright_relative: [f32; 3],
    nan: usize,
}

impl FrameStats {
    fn of(image: &Image) -> Self {
        let data = f32_plane(image);
        let mut lumas: Vec<(f32, [f32; 3])> = Vec::with_capacity(data.len() / 4);
        let mut nan = 0usize;
        for px in data.as_chunks::<4>().0 {
            if px.iter().any(|c| c.is_nan()) {
                nan += 1;
                continue;
            }
            let rgb = [px[0], px[1], px[2]];
            lumas.push((luma(rgb), rgb));
        }
        assert!(!lumas.is_empty(), "frame is empty");
        lumas.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        let n = lumas.len();
        let mean = lumas.iter().map(|(l, _)| l).sum::<f32>() / n as f32;
        let at = |q: f32| lumas[((n as f32 * q) as usize).min(n - 1)].0;
        let variance = lumas
            .iter()
            .map(|(l, _)| (l - mean) * (l - mean))
            .sum::<f32>()
            / n as f32;
        let mut total = [0.0f64; 3];
        for (_, rgb) in &lumas {
            for c in 0..3 {
                total[c] += rgb[c] as f64;
            }
        }
        let bright = &lumas[n - (n / 100).max(1)..];
        let mut bright_total = [0.0f64; 3];
        for (_, rgb) in bright {
            for c in 0..3 {
                bright_total[c] += rgb[c] as f64;
            }
        }
        let bright_mean = bright.iter().map(|(l, _)| l).sum::<f32>() / bright.len() as f32;
        let rel = |v: &[f64; 3], m: f32, count: usize| {
            [
                (v[0] / count as f64) as f32 / m.max(1e-9),
                (v[1] / count as f64) as f32 / m.max(1e-9),
                (v[2] / count as f64) as f32 / m.max(1e-9),
            ]
        };
        Self {
            mean,
            median: at(0.5),
            p10: at(0.1),
            p90: at(0.9),
            p99: at(0.99),
            relative_stddev: variance.sqrt() / mean.max(1e-9),
            relative: rel(&total, mean, n),
            bright_relative: rel(&bright_total, bright_mean, bright.len()),
            nan,
        }
    }
}

fn luma(rgb: [f32; 3]) -> f32 {
    0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2]
}

/// Converts the RGBA16Float texture data to f32 values.
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
                // Subnormal: normalize until the implicit leading 1 shows. `e`
                // is the f16 exponent field the value would have as a normal
                // (1), which shifts down once per left shift.
                let mut e: u32 = 1 + 112;
                let mut f = frac;
                while f & 0x0400 == 0 {
                    f <<= 1;
                    e -= 1;
                }
                f &= 0x03FF;
                sign | (e << 23) | (f << 13)
            }
        }
        0x1F => sign | 0x7F80_0000 | (frac << 13),
        _ => sign | (((exp as u32) + 112) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}
