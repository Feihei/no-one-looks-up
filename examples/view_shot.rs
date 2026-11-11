//! Dev tool: one first-person frame at an arbitrary pose, saved as a PNG.
//!
//! Usage (requires a GPU):
//!
//! ```text
//! X=51 Z=400 YAW=-90 PITCH=20 SHOT_OUT=target/shot.png \
//!   cargo run --example view_shot
//! ```
//!
//! `YAW` is degrees clockwise from north (-Z), `PITCH` degrees above the
//! horizon, `X`/`Z` world metres. The eye sits at 1.65 m like the walker.
//!
//! `PAN` (degrees per second) turns the camera at a constant rate instead of
//! holding it still, so the frame at the capture point is a *moving* frame --
//! the only way to see what a temporal filter does under motion.

use std::time::Duration;

use bevy::asset::AssetPlugin;
use bevy::camera::{CameraPlugin, Exposure, ImageRenderTarget, RenderTarget};
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
use no_one_looks_up::world::path_network::WorldSeed;

const IMAGE_SIZE: u32 = 1280;
const EYE: f32 = 1.65;

#[derive(Resource, Default)]
struct FrameCounter(usize);

#[derive(Resource, Default)]
struct CapturedFrame(Option<Image>);

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(default)
}

fn main() {
    let seed = std::env::var("SEED")
        .ok()
        .and_then(|raw| {
            let raw = raw.trim();
            match raw.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => raw.parse().ok(),
            }
        })
        .unwrap_or(no_one_looks_up::world::chunks::DEFAULT_SEED);
    let x = env_f32("X", 400.0);
    let z = env_f32("Z", 400.0);
    let yaw = env_f32("YAW", 0.0).to_radians();
    let pitch = env_f32("PITCH", 22.55).to_radians();
    let out = std::env::var("SHOT_OUT").unwrap_or_else(|_| "target/shot.png".to_string());
    let pan = env_f32("PAN", 0.0);

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
        bevy::world_serialization::WorldSerializationPlugin,
        bevy::gltf::GltfPlugin::default(),
        bevy::pbr::PbrPlugin::default(),
        bevy::light::LightPlugin,
        bevy::gizmos::GizmoPlugin,
        GamePlugins,
        LogPlugin::default(),
    ));
    app.finish();
    app.init_asset::<bevy::animation::AnimationClip>();
    app.insert_resource(WorldSeed(seed));
    app.init_resource::<FrameCounter>();
    app.init_resource::<CapturedFrame>();

    let target = {
        let mut images = app.world_mut().resource_mut::<Assets<Image>>();
        images.add(Image::new_target_texture(
            IMAGE_SIZE,
            IMAGE_SIZE * 9 / 16,
            TextureFormat::Rgba16Float,
            None,
        ))
    };

    let eye = Vec3::new(x, EYE, z);
    let dir = Vec3::new(
        yaw.sin() * pitch.cos(),
        pitch.sin(),
        -yaw.cos() * pitch.cos(),
    );
    app.world_mut().spawn((
        Name::new("ViewShotCamera"),
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
        Tonemapping::AcesFitted,
        Bloom::NATURAL,
        Transform::from_translation(eye).looking_at(eye + dir, Vec3::Y),
    ));
    app.insert_resource(Pose {
        eye,
        yaw,
        pitch,
        pan,
    });

    app.add_systems(
        Update,
        (
            bump_frame_counter,
            hold_camera,
            take_screenshot.run_if(in_frame(90)),
        )
            .chain(),
    );

    // Frame pacing. `SHOT_SLEEP=0` runs flat out, which is how the numbers in
    // the commit messages were taken: forty frames of wall clock with and
    // without a render feature, same pose, same seed.
    let sleep = Duration::from_millis(env_f32("SHOT_SLEEP", 16.0) as u64);
    let mut frames = 0usize;
    // Timing window: the 30 frames before the capture, so the one-off PNG
    // encode and the screenshot's own readback do not land in the average.
    let mut window: Option<std::time::Instant> = None;
    let mut window_frames = 0usize;
    let mut frame_ms = f64::NAN;
    loop {
        app.update();
        frames += 1;
        std::thread::sleep(sleep);
        if frames == 60 {
            window = Some(std::time::Instant::now());
            window_frames = 0;
        }
        if let Some(at) = window {
            window_frames += 1;
            frame_ms = at.elapsed().as_secs_f64() * 1000.0 / window_frames as f64;
        }
        if frames.is_multiple_of(30) {
            println!("frame {frames}");
        }
        if app.world().resource::<CapturedFrame>().0.is_some() {
            for _ in 0..30 {
                app.update();
                std::thread::sleep(sleep);
            }
            println!(
                "saved {out} (seed {seed:#x}, eye {eye:?}, yaw {:.1}, pitch {:.1}, \
                 {frame_ms:.2} ms/frame)",
                yaw.to_degrees(),
                pitch.to_degrees(),
            );
            return;
        }
        assert!(frames < 400, "shot not captured within 400 frames");
    }
}

/// The camera's pose. Held rather than driven by the walk controller, and
/// optionally turned at a constant rate so a moving shot can be compared with
/// a still one -- that is the only way to see a temporal filter's ghosting.
#[derive(Resource)]
struct Pose {
    eye: Vec3,
    yaw: f32,
    pitch: f32,
    /// Degrees per second, clockwise (positive = to the right).
    pan: f32,
}

fn bump_frame_counter(mut counter: ResMut<FrameCounter>) {
    counter.0 += 1;
}

fn in_frame(frame: usize) -> impl FnMut(Res<FrameCounter>) -> bool + Clone {
    move |counter: Res<FrameCounter>| counter.0 == frame
}

/// The walk controller would move the camera from the keyboard; clear the
/// input so the pose stays where it was put, then put the pose back -- the
/// controller integrates `Transform` every frame whether or not a key is held,
/// so position has to be re-asserted, not just frozen.
///
/// With `PAN` set the yaw is a function of the frame counter, not of wall-clock
/// time, so two runs (TAA on and off) reach frame 90 at the same pose and the
/// images can be compared pixel for pixel.
fn hold_camera(
    counter: Res<FrameCounter>,
    pose: Res<Pose>,
    mut input: ResMut<ButtonInput<KeyCode>>,
    mut cameras: Query<&mut Transform, With<Camera3d>>,
) {
    input.clear();
    let yaw = pose.yaw + pose.pan.to_radians() * (counter.0 as f32 / 60.0);
    let dir = Vec3::new(
        yaw.sin() * pose.pitch.cos(),
        pose.pitch.sin(),
        -yaw.cos() * pose.pitch.cos(),
    );
    for mut transform in &mut cameras {
        *transform = Transform::from_translation(pose.eye).looking_at(pose.eye + dir, Vec3::Y);
    }
}

fn take_screenshot(mut commands: Commands, targets: Query<&RenderTarget, With<Camera3d>>) {
    let Ok(target) = targets.single() else {
        return;
    };
    let RenderTarget::Image(image_target) = target else {
        return;
    };
    let handle = image_target.handle.clone();
    let out = std::env::var("SHOT_OUT").unwrap_or_else(|_| "target/shot.png".to_string());
    if let Some(parent) = std::path::Path::new(&out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entity = commands
        .spawn(Screenshot::image(handle))
        .observe(
            |trigger: On<ScreenshotCaptured>, mut frame: ResMut<CapturedFrame>| {
                frame.0 = Some(trigger.image.clone());
            },
        )
        .id();
    commands
        .entity(entity)
        .observe(bevy::render::view::screenshot::save_to_disk(out));
}
