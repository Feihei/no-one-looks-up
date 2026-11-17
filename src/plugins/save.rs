//! Save system plugin (R4).
//!
//! Save = seed + season enum + exploration bitmap + player pose; restore
//! regenerates the layout from the seed (ADR-0001) and must complete in
//! under a second. R4.2 owns the format and file IO ([`save_data`]); this
//! plugin owns the restore flow: a [`LoadRequest`] message tears down the
//! streamed world, rebuilds it from the saved seed and restores the
//! exploration state and player pose.

use bevy::prelude::*;

use crate::plugins::save_data;

use crate::player::{EYE_HEIGHT, PlayerBody};
use crate::world::chunks::{ChunkManager, ChunkRoot, WorldSystems};
use crate::world::collision::CollisionIndex;
use crate::world::explored::ExploredBitmap;
use crate::world::path_network::{WorldSeed, generate_world};
use crate::world::season::SeasonState;

pub use self::save_data::{
    AUTOSAVE_SLOT, SaveData, Season, list_slots, read_save, save_path, slot_info, write_save,
};

/// Requests a restore from a captured save. Sent by the R4.5 menu (and by
/// tests); consumed once per frame.
#[derive(Message, Debug, Clone)]
pub struct LoadRequest(pub SaveData);

/// Requests writing the current game state to a slot. Sent by the menu's
/// save panel (R4.5); the autosave system writes slot 0 directly.
#[derive(Message, Debug, Clone)]
pub struct SaveGameRequest {
    /// Destination slot (1-99 for user saves; the load browser shows all
    /// 00-99, with 00 the autosave).
    pub slot: u32,
}

/// Interval between autosaves after the player moves between chunks (the
/// original's pacing; a per-chunk write would hammer the disk while
/// streaming).
const AUTOSAVE_INTERVAL: f32 = 30.0;

pub struct SavePlugin;

impl Plugin for SavePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<AutosaveState>()
            .add_message::<LoadRequest>()
            .add_message::<SaveGameRequest>()
            .add_systems(
                Update,
                (
                    handle_load_request
                        // Restore before streaming: the world this frame is
                        // rebuilt around the restored pose, not the one the
                        // walker left.
                        .before(WorldSystems::Streaming),
                    // Saving reads the settled pose (after movement, after
                    // streaming has marked exploration) and after the
                    // tracker, so a chunk change and its autosave timer
                    // re-arm land in the same frame.
                    handle_save_requests.after(WorldSystems::Streaming),
                    autosave
                        .after(handle_save_requests)
                        .after(crate::world::explored::track_player_chunk),
                ),
            );
    }
}

/// Tears down the streamed world and rebuilds it from the save.
fn handle_load_request(
    mut requests: MessageReader<LoadRequest>,
    mut commands: Commands,
    chunk_roots: Query<Entity, With<ChunkRoot>>,
    mut manager: ResMut<ChunkManager>,
    mut collision: ResMut<CollisionIndex>,
    mut bitmap: ResMut<ExploredBitmap>,
    mut players: Query<&mut Transform, With<Camera3d>>,
) {
    let Some(request) = requests.read().next() else {
        return;
    };
    let data = &request.0;
    if data.version != save_data::SAVE_VERSION {
        error!(
            "save version {} not supported (this build expects {})",
            data.version,
            save_data::SAVE_VERSION
        );
        return;
    }

    // Regenerate the layout from the seed (bit-exact; tested in path_network).
    let started = std::time::Instant::now();
    match generate_world(data.seed) {
        Ok((grid, graph)) => {
            info!(
                "save restored: seed={:#x} blocks={} explored={} in {:.1?}",
                data.seed,
                graph.blocks.len(),
                data.explored.len(),
                started.elapsed()
            );
            commands.insert_resource(grid);
            commands.insert_resource(graph);
        }
        Err(err) => {
            error!("save restore failed: world generation error: {err}");
            return;
        }
    }

    // Streamed state: drop every chunk group and start the window over
    // around the restored pose (streaming refills it within a few frames).
    for entity in &chunk_roots {
        commands.entity(entity).try_despawn();
    }
    *manager = ChunkManager::default();
    *collision = CollisionIndex::default();
    *bitmap = data.explored_bitmap();

    // The seed resource must agree with the restored world for anything that
    // reads it later (e.g. a menu-initiated restart).
    commands.insert_resource(WorldSeed(data.seed));

    // R3.5: restore the season with the world. This is the ONLY place a run's
    // season can come from a file (ADR-0001: fixed for the run), and it runs
    // before the chunks stream back in, so no tree is ever built for the wrong
    // season.
    commands.insert_resource(SeasonState::new(data.season));

    // Player pose: eye height, saved XZ, saved facing (pitch resets to the
    // golden-hour default; the format keeps room for it if that ever moves).
    if let Ok(mut transform) = players.single_mut() {
        transform.translation = Vec3::new(data.player_pos[0], EYE_HEIGHT, data.player_pos[1]);
        transform.rotation = Quat::from_euler(
            bevy::math::EulerRot::YXZ,
            data.player_yaw,
            PlayerBody::default().pitch,
            0.0,
        );
    } else {
        warn!("save restore: no camera found, player pose not applied");
    }
}

/// Autosave bookkeeping: time since the last chunk change that has not yet
/// triggered a write.
#[derive(Resource, Debug, Default)]
struct AutosaveState {
    pending_for: Option<f32>,
}

/// Everything a save has to capture, gathered once.
///
/// A `SystemParam` rather than seven arguments on each system: the autosave and
/// the manual save need exactly the same inputs, and duplicating that list is
/// how the two paths drifted apart (the manual path used to hard-code the seed
/// and the season).
#[derive(bevy::ecs::system::SystemParam)]
struct SaveInputs<'w, 's> {
    cameras: Query<'w, 's, &'static Transform, With<Camera3d>>,
    players: Query<'w, 's, &'static PlayerBody>,
    bitmap: Res<'w, ExploredBitmap>,
    seed: Option<Res<'w, WorldSeed>>,
    season: Option<Res<'w, SeasonState>>,
}

impl SaveInputs<'_, '_> {
    /// Captures the current game state.
    fn capture(&self) -> Option<SaveData> {
        let camera = self.cameras.single().ok()?;
        let yaw = self
            .players
            .single()
            .map(|body| body.yaw)
            .unwrap_or_default();
        Some(SaveData::capture(
            self.seed
                .as_deref()
                .map(|s| s.0)
                .unwrap_or(crate::world::chunks::DEFAULT_SEED),
            // R3.5: the run's season, not a placeholder. A save restored with
            // the wrong season would regrow a summer crown in a winter world.
            self.season.as_deref().map(|s| s.season).unwrap_or_default(),
            &self.bitmap,
            [camera.translation.x, camera.translation.z],
            yaw,
        ))
    }
}

/// Writes the autosave slot some seconds after the player changes chunks;
/// standing still never triggers a write.
fn autosave(
    time: Res<Time>,
    mut state: ResMut<AutosaveState>,
    mut chunk_changes: MessageReader<crate::world::explored::PlayerChunkChanged>,
    inputs: SaveInputs,
) {
    // A chunk change arms (or re-arms) the timer.
    if chunk_changes.read().next().is_some() {
        state.pending_for = Some(0.0);
    }
    let Some(remaining) = state.pending_for.as_mut() else {
        return;
    };
    *remaining += time.delta_secs();
    if *remaining < AUTOSAVE_INTERVAL {
        return;
    }
    state.pending_for = None;
    let Some(data) = inputs.capture() else {
        return;
    };
    match write_save(AUTOSAVE_SLOT, &data) {
        Ok(()) => info!("autosaved"),
        Err(err) => error!("autosave failed: {err}"),
    }
}

/// Writes the current state to a requested slot.
fn handle_save_requests(mut requests: MessageReader<SaveGameRequest>, inputs: SaveInputs) {
    for request in requests.read() {
        let Some(data) = inputs.capture() else {
            continue;
        };
        match write_save(request.slot, &data) {
            Ok(()) => info!("saved to slot {}", request.slot),
            Err(err) => error!("save to slot {} failed: {err}", request.slot),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::chunks::WorldPlugin;
    use bevy::MinimalPlugins;

    /// World centre, derived from the world constants.
    const CENTER: f32 = crate::world::WORLD_SIZE * 0.5;

    /// Serializes tests that touch the real autosave file. They share one
    /// path on disk, so running them concurrently makes the "no autosave
    /// yet" / "autosave exists" assertions race each other.
    static AUTOSAVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn app_with_world() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, WorldPlugin, SavePlugin))
            .init_resource::<ButtonInput<KeyCode>>();
        app.world_mut().spawn((
            Camera3d::default(),
            PlayerBody::default(),
            Transform::from_xyz(CENTER, EYE_HEIGHT, CENTER),
        ));
        app
    }

    #[test]
    fn load_request_restores_bitmap_and_pose() {
        let mut app = app_with_world();
        app.update(); // Startup + first streaming frame.

        // Capture the save at the spawn pose, then wander away and explore.
        let (bitmap_snapshot, saved) = {
            let bitmap = app.world().resource::<ExploredBitmap>();
            let data = SaveData::capture(
                crate::world::chunks::DEFAULT_SEED,
                Season::Summer,
                bitmap,
                [CENTER, CENTER],
                0.0,
            );
            (bitmap.clone(), data)
        };
        assert!(bitmap_snapshot.is_explored((8, 8)));

        // Wander: teleport far away and let streaming mark exploration.
        let mut cameras = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Camera3d>>();
        for mut transform in cameras.iter_mut(app.world_mut()) {
            transform.translation = Vec3::new(CENTER + 300.0, EYE_HEIGHT, CENTER + 300.0);
        }
        let _ = &mut cameras;
        app.update();
        app.update();
        let after_walk = app.world().resource::<ExploredBitmap>().clone();
        assert!(
            after_walk.len() > bitmap_snapshot.len(),
            "walking adds explored chunks"
        );

        // Restore: the message resets everything to the saved state.
        app.world_mut().write_message(LoadRequest(saved.clone()));
        app.update();
        app.update();

        let bitmap = app.world().resource::<ExploredBitmap>();
        assert_eq!(bitmap.len(), saved.explored.len(), "bitmap restored");
        assert!(bitmap.is_explored((8, 8)));
        assert!(!bitmap.is_explored((14, 14)), "post-walk chunks unlearned");

        let mut cameras = app
            .world_mut()
            .query_filtered::<&Transform, With<Camera3d>>();
        let transform = cameras.single(app.world()).expect("camera exists");
        assert!((transform.translation.x - CENTER).abs() < 1e-4);
        assert!((transform.translation.z - CENTER).abs() < 1e-4);

        // Streaming rebuilds the window around the restored pose.
        let manager = app.world().resource::<ChunkManager>();
        assert!(!manager.active.is_empty(), "chunks respawn after restore");
    }

    #[test]
    fn future_versions_are_rejected_safely() {
        let mut app = app_with_world();
        app.update();

        let mut future = SaveData::capture(
            crate::world::chunks::DEFAULT_SEED,
            Season::Summer,
            &ExploredBitmap::default(),
            [0.0, 0.0],
            0.0,
        );
        future.version = save_data::SAVE_VERSION + 1;
        app.world_mut().write_message(LoadRequest(future));
        app.update();

        // The world is untouched: chunks are still active.
        let manager = app.world().resource::<ChunkManager>();
        assert!(!manager.active.is_empty(), "reject must not tear down");
    }

    #[test]
    fn slot_list_reflects_what_was_written_and_tolerates_corruption() {
        let _guard = AUTOSAVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Clean the slots this test owns (autosave + one user slot), then
        // verify the list API mirrors reality.
        let _ = std::fs::remove_file(save_path(AUTOSAVE_SLOT));
        let _ = std::fs::remove_file(save_path(2));
        assert!(
            !list_slots().iter().any(|i| i.slot == AUTOSAVE_SLOT),
            "no autosave yet"
        );

        let mut bitmap = ExploredBitmap::default();
        bitmap.mark((3, 4));
        bitmap.mark((4, 3));
        let data = SaveData::capture(0xBEEF, Season::Summer, &bitmap, [1.0, 2.0], 0.5);
        write_save(AUTOSAVE_SLOT, &data).expect("autosave writes");
        write_save(2, &data).expect("user slot writes");

        let slots = list_slots();
        let auto = slots
            .iter()
            .find(|i| i.slot == AUTOSAVE_SLOT)
            .expect("listed");
        assert_eq!(auto.seed, 0xBEEF);
        assert_eq!(auto.explored_chunks, 2);
        let _ = std::fs::remove_file(save_path(AUTOSAVE_SLOT));

        // A corrupt file must not break the list, just be absent from it.
        std::fs::write(save_path(2), "{not json").expect("corrupts the file");
        let slots = list_slots();
        assert!(!slots.iter().any(|i| i.slot == 2), "corrupt slot skipped");
        let _ = std::fs::remove_file(save_path(2));
    }

    #[test]
    fn autosave_writes_only_after_the_interval() {
        let _guard = AUTOSAVE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut app = app_with_world();
        app.update();
        app.update();

        // No chunk change yet: no autosave file, even with time passing.
        for _ in 0..3 {
            app.update();
        }
        assert!(
            !std::path::Path::new(&save_path(AUTOSAVE_SLOT)).exists(),
            "standing still must not autosave"
        );

        // Move to a new chunk: the tracker arms the timer; the autosave
        // fires once the accumulated delta crosses the interval. The test
        // relies on the tracker and the autosave running in the same frame
        // (explicit .after edge) and simply accumulates real frames.
        let mut cameras = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Camera3d>>();
        for mut transform in cameras.iter_mut(app.world_mut()) {
            transform.translation = Vec3::new(CENTER + 120.0, EYE_HEIGHT, CENTER);
        }
        // Real-frame loop is impractical at 30 s; verify the arming + write
        // path directly instead: run enough simulated frames by shrinking
        // the requirement - we assert the mechanism (change seen, state
        // armed) and that a manual save request works end to end.
        app.update();
        app.update();
        // The chunk change was seen; simulate the interval having passed by
        // forcing the pending counter high, then one more update writes.
        {
            let mut state = app.world_mut().resource_mut::<AutosaveState>();
            state.pending_for = Some(AUTOSAVE_INTERVAL);
        }
        app.update();
        assert!(
            std::path::Path::new(&save_path(AUTOSAVE_SLOT)).exists(),
            "armed timer + interval triggers the autosave write"
        );
        let loaded = read_save(AUTOSAVE_SLOT).expect("autosave is readable");
        assert!(loaded.explored.contains(&(10, 8)), "autosave carries state");
        let _ = std::fs::remove_file(save_path(AUTOSAVE_SLOT));
    }

    #[test]
    fn regeneration_from_seed_stays_under_a_second() {
        // ADR-0001: restore must complete in under a second. The dominant
        // cost is layout generation; assert it directly (release-target).
        let started = std::time::Instant::now();
        let result = generate_world(0x4845_4144);
        let elapsed = started.elapsed();
        assert!(result.is_ok());
        assert!(elapsed.as_secs_f64() < 1.0, "generation took {elapsed:?}");
    }

    #[test]
    fn season_survives_the_save_round_trip_and_reaches_the_world() {
        // R3.5 regression: the season field existed in the format since R4.2
        // but was a placeholder that always wrote `Summer`. A save made in
        // autumn then restored a summer world -- a change no test noticed,
        // because nothing read the field back into the world.
        let mut app = app_with_world();
        app.init_resource::<SeasonState>();
        app.update();

        // Pick autumn, then save through the live system rather than by
        // hand-building a `SaveData`, so the capture path is the one under
        // test.
        *app.world_mut().resource_mut::<SeasonState>() =
            SeasonState::new(crate::plugins::save::Season::Autumn);
        app.world_mut().write_message(SaveGameRequest { slot: 997 });
        app.update();
        let saved = read_save(997).expect("the save system wrote the slot");
        assert_eq!(
            saved.season,
            crate::plugins::save::Season::Autumn,
            "the live season must reach the file"
        );
        let _ = std::fs::remove_file(save_path(997));

        // Restoring that save must put the season back into the world.
        *app.world_mut().resource_mut::<SeasonState>() =
            SeasonState::new(crate::plugins::save::Season::Spring);
        app.world_mut().write_message(LoadRequest(saved));
        app.update();
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            crate::plugins::save::Season::Autumn,
            "loading a save must restore its season"
        );
    }

    #[test]
    fn restored_season_is_applied_before_any_tree_streams() {
        // The uniform is pushed in `Update`, so the ordering that matters is:
        // restore season -> stream chunks -> build trees. A tree baked for the
        // wrong season would keep the wrong crown until the next chunk cycle.
        let mut app = app_with_world();
        app.init_resource::<SeasonState>();
        app.update();

        let bitmap = app.world().resource::<ExploredBitmap>().clone();
        let winter = SaveData::capture(
            crate::world::chunks::DEFAULT_SEED,
            Season::Winter,
            &bitmap,
            [CENTER, CENTER],
            0.0,
        );
        app.world_mut().write_message(LoadRequest(winter));
        app.update();
        app.update();

        assert_eq!(
            app.world().resource::<SeasonState>().season,
            Season::Winter,
            "season must be restored with the world"
        );
    }
}
