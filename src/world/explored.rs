//! Exploration tracking (R4.1, ADR-0001: the original's exploration bitmap).
//!
//! A chunk counts as explored once the player has had it inside the active
//! window — standing in it is not required, matching the original's
//! "walked past" semantics. The set is a plain `HashSet` of chunk keys so the
//! R4.2 save can serialize it directly and regeneration stays bit-exact:
//! the same walk always marks the same keys.

use bevy::prelude::*;
use std::collections::HashSet;

use crate::world::GRID_CELLS;
use crate::world::collision::chunk_key;

/// Chunk keys the player has seen, at 50 m chunk granularity.
#[derive(Resource, Debug, Default, Clone)]
pub struct ExploredBitmap {
    explored: HashSet<(i32, i32)>,
}

impl ExploredBitmap {
    /// Marks one chunk explored. Returns true when it was new.
    pub fn mark(&mut self, key: (i32, i32)) -> bool {
        self.explored.insert(key)
    }

    /// True if the chunk has been explored.
    pub fn is_explored(&self, key: (i32, i32)) -> bool {
        self.explored.contains(&key)
    }

    /// Number of explored chunks.
    pub fn len(&self) -> usize {
        self.explored.len()
    }

    /// True when nothing is explored yet.
    pub fn is_empty(&self) -> bool {
        self.explored.is_empty()
    }

    /// Total number of chunks in the world: the exploration denominator.
    pub fn total(&self) -> usize {
        (GRID_CELLS * GRID_CELLS) as usize
    }

    /// Fraction of the world explored (0.0-1.0).
    pub fn fraction(&self) -> f32 {
        self.explored.len() as f32 / self.total() as f32
    }

    /// True once every chunk has been visited (R4.6 completion gate).
    pub fn is_complete(&self) -> bool {
        self.explored.len() >= self.total()
    }

    /// Explored keys, sorted, for deterministic serialization (R4.2).
    pub fn sorted_keys(&self) -> Vec<(i32, i32)> {
        let mut keys: Vec<_> = self.explored.iter().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Restores a previously saved bitmap (R4.3); keys are trusted input.
    pub fn restore(keys: impl IntoIterator<Item = (i32, i32)>) -> Self {
        ExploredBitmap {
            explored: keys.into_iter().collect(),
        }
    }
}

/// Fires once every chunk has been explored (R4.6 completion gate).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldExplorationCompleted;

/// Message sent when the player's chunk changed (R4.1: the save system
/// listens to mark exploration; R4.6 may also use it for autosave timing).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerChunkChanged {
    /// The chunk the player is now in.
    pub new_chunk: (i32, i32),
    /// The chunk the player left (None on the first frame).
    pub old_chunk: Option<(i32, i32)>,
}

/// Marks the player's current chunk explored and sends the change message.
///
/// Called from the streaming system (it already computes the camera
/// position); tracking inside one system keeps the semantics single-sourced.
pub(crate) fn track_player_chunk(
    cameras: Query<&Transform, With<Camera3d>>,
    mut bitmap: ResMut<ExploredBitmap>,
    mut last: Local<Option<(i32, i32)>>,
    mut changed: MessageWriter<PlayerChunkChanged>,
) {
    let Ok(camera) = cameras.single() else {
        return;
    };
    let key = chunk_key(Vec2::new(camera.translation.x, camera.translation.z));
    // Only touch the resource when the chunk is genuinely new. `mark` takes
    // `&mut self`, and change detection cannot tell a no-op insert from a real
    // edit: marking every frame flagged `ExploredBitmap` as changed on every
    // tick, which made the HUD's rebuild guard respawn the whole panel each
    // frame (and re-lay out its Chinese line, printing an ICU4X warning per
    // layout in debug builds).
    if !bitmap.is_explored(key) {
        bitmap.mark(key);
    }
    if *last != Some(key) {
        changed.write(PlayerChunkChanged {
            new_chunk: key,
            old_chunk: *last,
        });
        *last = Some(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_tracks_new_keys_once() {
        let mut bitmap = ExploredBitmap::default();
        assert!(bitmap.mark((3, 4)), "first visit is new");
        assert!(!bitmap.mark((3, 4)), "revisit is not");
        assert!(bitmap.is_explored((3, 4)));
        assert_eq!(bitmap.len(), 1);
        assert!(bitmap.fraction() > 0.0);
        assert!(!bitmap.is_complete());
    }

    #[test]
    fn revisiting_a_chunk_does_not_touch_the_bitmap() {
        use bevy::ecs::change_detection::DetectChanges;

        let mut app = App::new();
        app.init_resource::<ExploredBitmap>()
            .add_message::<PlayerChunkChanged>()
            .add_systems(Update, track_player_chunk);
        app.world_mut()
            .spawn((Camera3d::default(), Transform::default()));

        app.update(); // first tick marks the chunk the camera stands in
        assert_eq!(app.world().resource::<ExploredBitmap>().len(), 1);

        // Standing still must not re-flag the resource: the HUD rebuild guard
        // keys off `is_changed`, and a per-frame flag respawns the panel every
        // frame.
        app.world_mut().clear_trackers();
        app.update();
        assert!(
            !app.world().resource_ref::<ExploredBitmap>().is_changed(),
            "an already-explored chunk must not mark the bitmap as changed"
        );
    }

    #[test]
    fn completion_requires_every_chunk() {
        let mut bitmap = ExploredBitmap::default();
        let total = (GRID_CELLS * GRID_CELLS) as i32;
        for i in 0..total {
            bitmap.mark((i % GRID_CELLS as i32, i / GRID_CELLS as i32));
        }
        assert!(bitmap.is_complete());
        assert!((bitmap.fraction() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sorted_keys_round_trip_is_bit_exact() {
        let mut bitmap = ExploredBitmap::default();
        for k in [(7_i32, 2_i32), (0, 0), (39, 39), (7, 2)] {
            bitmap.mark(k);
        }
        let restored = ExploredBitmap::restore(bitmap.sorted_keys());
        assert_eq!(bitmap.len(), restored.len());
        for key in bitmap.sorted_keys() {
            assert!(restored.is_explored(key));
        }
        assert_eq!(bitmap.sorted_keys(), vec![(0, 0), (7, 2), (39, 39)]);
    }
}
