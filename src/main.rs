//! No One Looks Up Even Alone: a first-person endless strolling experience focused on the
//! golden-hour sky. Bevy 0.20 migration of the original Godot 4.7 project.
//!
//! R2: world generation + chunk streaming (ground tiles stream around the
//! camera; debug gizmos toggle with G). R3.4: kinematic player controller
//! (A/D turn, W/S walk, mouse look clamped to -10°-60°, E/Q speed).
//! Placement/seasons (R3) and save/menu (R4) per milestone.

use bevy::camera::Exposure;
use bevy::prelude::*;

use no_one_looks_up::player::{EYE_HEIGHT, PlayerBody, PlayerPlugin};
use no_one_looks_up::plugins::{GamePlugins, GoldenHourCamera, sky::DuskSky};
use no_one_looks_up::world::world_center;

fn main() -> AppExit {
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "No One Looks Up Even Alone".to_string(),
                    ..default()
                }),
                ..default()
            }),
            GamePlugins,
            PlayerPlugin,
            // GPU timings per pass (the clouds' noise/weather/march/resolve/
            // composite spans feed the diagnostics overlay).
            bevy::render::diagnostic::RenderDiagnosticsPlugin,
        ))
        .add_systems(Startup, (setup_scene, log_sky_state.after(setup_scene)))
        .run()
}

/// The player: one entity carrying both the camera and the kinematic body
/// (R3.4), spawned at the world centre on street level.
///
/// `PlayerBody` owns the camera's rotation from now on — the R1 `aim_at_sun`
/// helper is gone, because two systems writing one Transform is exactly the
/// implicit-ordering trap the project forbids. The controller starts pitched
/// at the top of the allowed band (60°) so the golden-hour sky fills the view
/// the way ADR-0001 describes.
fn setup_scene(mut commands: Commands, sky: Res<DuskSky>) {
    commands.spawn((
        Name::new("Player"),
        Camera3d::default(),
        GoldenHourCamera {
            exposure: Exposure {
                ev100: sky.exposure_ev100,
            },
            ..default()
        },
        PlayerBody::default(),
        Transform::from_xyz(world_center(), EYE_HEIGHT, world_center()),
    ));
}

/// One-shot startup log: confirms the golden-hour lock took effect. The first
/// frame's `SunState` is still the default (update runs after Startup), so
/// `report_sun_altitude` shows the settled ephemeris.
fn log_sky_state(sky: Res<DuskSky>) {
    info!(
        "golden hour locked: date={:?} hour={} loc=({:.4}, {:.4}, UTC+{})",
        sky.date,
        sky.golden_hour,
        sky.location.latitude_deg,
        sky.location.longitude_deg,
        sky.location.utc_offset_hours
    );
}
