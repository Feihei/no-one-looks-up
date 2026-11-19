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
use no_one_looks_up::world::layout_graph::LayoutGraph;
use no_one_looks_up::world::path_network::{WorldSeed, spawn_point};
use no_one_looks_up::world::world_center;

fn main() -> AppExit {
    App::new()
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    // Player-facing text is Chinese throughout (menus, HUD);
                    // the window title follows.
                    title: "即使独自漫步你也很少看天".to_string(),
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
        .add_systems(
            Update,
            place_player_at_spawn.before(no_one_looks_up::world::chunks::WorldSystems::Streaming),
        )
        .run()
}

/// The player: one entity carrying both the camera and the kinematic body
/// (R3.4), spawned at the world centre on street level as a fallback —
/// [`place_player_at_spawn`] moves it onto the seed-picked junction (R3.20)
/// as soon as the layout exists.
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

/// R3.20 spawn (2026-11-19 user decision: 出生点随机指定一个路口点，避免落地
/// 块内): one-shot placement of the player on a seed-picked junction. Runs in
/// `Update` because the layout graph is inserted by `generate_layout`'s
/// commands, which no same-frame Startup system can see yet. Run starts go
/// through the R4.3 restore flow (menu `start_run` captures the same
/// junction pose), so this only fixes where the player stands before and at
/// the first frame of a run.
fn place_player_at_spawn(
    graph: Option<Res<LayoutGraph>>,
    seed: Option<Res<WorldSeed>>,
    mut players: Query<(&mut Transform, &mut PlayerBody), With<Camera3d>>,
    mut placed: Local<bool>,
) {
    if *placed {
        return;
    }
    let Some(graph) = graph else {
        return;
    };
    let seed = seed
        .map(|s| s.0)
        .unwrap_or(no_one_looks_up::world::chunks::DEFAULT_SEED);
    let (pos, yaw) = spawn_point(&graph, seed);
    let Ok((mut transform, mut body)) = players.single_mut() else {
        return;
    };
    transform.translation = Vec3::new(pos.x, EYE_HEIGHT, pos.y);
    body.yaw = yaw;
    *placed = true;
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
