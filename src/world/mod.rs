//! World generation (R2): layout graph, block-first path network, and chunk
//! streaming, ported from the original M2/M3 algorithms (ADR-0002/0004).
//!
//! Pipeline (ADR-0002):
//!
//! ```text
//! seed -> path_network::generate_world -> (WorldGrid, LayoutGraph)
//!              (startup, <1s, no 3D meshes)
//! player position -> chunks::ChunkManager -> spawn/despawn chunk groups
//! ```

pub mod chunks;
pub mod collision;
pub mod debug_viz;
pub mod explored;
pub mod facade;
pub mod geometry;
pub mod layout_graph;
pub mod leaf_shape;
pub mod overhead;
pub mod path_network;
pub mod placement;
pub mod props;
pub mod roof;
pub mod season;
pub mod tree_mesh;
pub mod tree_shape;
pub mod world_grid;

/// Base grid cell edge, in meters. The world is `GRID_CELLS` x `GRID_CELLS`
/// cells of this size; blocks are merged cell groups and streets run along the
/// grid lines between blocks (ADR-0004: block-first, streets are negative
/// space).
pub const CELL_SIZE: f32 = 50.0;

/// Grid cells per axis. 16 x 16 cells = 0.8 km x 0.8 km world.
///
/// Sizing (ADR-0001): the walk is 1.2-2.0 m/s, so half an hour covers
/// ~2.7 km at the default 1.5 m/s. At 800 m a player can cross the map
/// (~1.6 km) or walk most of its perimeter (3.2 km) within that window,
/// while the city still reads as a city from the shoulders-up view.
pub const GRID_CELLS: u32 = 16;

/// Total world extent in meters (`CELL_SIZE * GRID_CELLS`).
pub const WORLD_SIZE: f32 = CELL_SIZE * GRID_CELLS as f32;

/// Outermost ring(s) of cells forced to be solid blocks (OLD_CITY/APARTMENT),
/// forming the enclosure wall around the world (ADR-0004).
pub const OUTER_BAND_CELLS: u32 = 1;

/// Nominal street width (original `STREET_WIDTH`): the ring road's width,
/// used where an outer block retreats from the enclosure ring. Outer blocks
/// must all use the **same** value — their corners meet at shared nodes, and
/// two different half-widths there would open a gap in the wall.
pub const STREET_WIDTH: f32 = 8.0;

/// Per-block street half-width range. Each block stores a random street width
/// in `[MIN, MAX]` and insets by `width / 2`; adjacent blocks share the street
/// centerline, so the full street width between two blocks is
/// `(width_a + width_b) / 2` (ADR-0004).
pub const STREET_WIDTH_MIN: f32 = 6.0;
pub const STREET_WIDTH_MAX: f32 = 10.0;

/// Maximum lattice-node perturbation on each axis, in meters.
///
/// The original used 12 m, but at a 50 m cell pitch that lets two adjacent
/// nodes drift 24 m closer or further apart, tilting a shared edge up to 53°
/// off axis. Insetting such an edge by its half street width then pushes
/// vertices ~5.4 m sideways — more than the street itself — so neighbouring
/// blocks interpenetrate (measured: 286 pairs, up to 20.8 m deep).
///
/// 4 m keeps the tilt near 22°, where a 4 m inset shifts vertices ~1.6 m:
/// comfortably under the halved street width, so blocks can no longer meet.
/// The streets still wobble visibly (edges range 34-68 m instead of 27-74 m).
/// Interior nodes wobble; map-border nodes stay exact.
pub const PERTURB_MAX: f32 = 4.0;

/// Height of the invisible solid collision prism (R3). Defined here because
/// the layout graph is its input.
pub const BLOCK_SOLID_HEIGHT: f32 = 2.0;

/// FOREST blocks are edge transition zones (ADR-0004): only cells within this
/// many cells of the map border may roll the FOREST type.
///
/// Kept at 1/4 of the map so the transition band stays a band: at 8 cells it
/// covered 20% of a 40-cell map, but a third of a 16-cell one.
pub const FOREST_EDGE_MAX_DISTANCE_CELLS: u32 = 4;

/// Chunk edge, in meters. Chunks align 1:1 with base grid cells
/// (800 / 50 = 16 x 16 chunks, ADR-0002).
pub const CHUNK_SIZE: f32 = 50.0;

/// World centre in world units. Spawn poses default here instead of a
/// hard-coded literal, so resizing [`GRID_CELLS`] keeps the spawn central.
pub fn world_center() -> f32 {
    WORLD_SIZE * 0.5
}

/// Streaming radius: chunks loaded ahead of the walk direction (ADR-0002,
/// first-person view optimization).
///
/// 4 chunks ahead is a 200 m sightline. On the previous 40-chunk map that
/// left a 20-chunk interior; on a 16-chunk map it is a quarter of the map,
/// which is still shorter than the sky-dominated view distance, so the
/// window stays comfortably inside the map while the player is central.
pub const CHUNK_AHEAD: i32 = 4;
/// Streaming radius: chunks kept behind the walk direction (turn safety).
pub const CHUNK_BEHIND: i32 = 2;
// Streaming window: the original's square scan, `chebyshev(dx, dz) <= FRONT`
// ahead and `<= BEHIND` behind, decided per chunk by the dot product with the
// walk direction (no separate side radius — the side extent IS the front
// radius, exactly like `chunk_manager.gd`).

/// Chunk loads/unloads processed per frame (original `CHUNKS_PER_FRAME`).
///
/// Spawning a chunk builds its contents; doing all 64 in one frame after a
/// teleport or a sharp turn would stall. One per frame spreads the burst over
/// ~64 frames while the player is still walking into it.
pub const CHUNKS_PER_FRAME: usize = 1;

#[cfg(test)]
mod tests {
    //! R2.8: scheduling property testing — the world generation + chunk
    //! streaming + debug visualization systems are registered in explicit
    //! `.before()`/`.after()` chains, so a schedule built in ANY registration
    //! order must initialize without scheduling ambiguities.

    use bevy::ecs::schedule::{LogLevel, Schedule, ScheduleBuildSettings};
    use bevy::prelude::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    use crate::player::{PlayerSystems, walk_and_collide};
    use crate::world::chunks::{WorldSystems, update_chunk_streaming};
    use crate::world::debug_viz::{
        DebugVizEnabled, draw_debug_viz, spawn_block_labels, toggle_debug_viz,
    };

    #[test]
    fn world_systems_have_no_scheduling_ambiguities_in_any_order() {
        for seed in 0..16u64 {
            // Fisher–Yates shuffle of the registration order, seeded.
            let mut order = [0usize, 1, 2, 3, 4];
            let mut rng = StdRng::seed_from_u64(seed);
            for i in (1..order.len()).rev() {
                let j = rng.random_range(..=i);
                order.swap(i, j);
            }

            let mut schedule = Schedule::default();
            schedule.set_build_settings(ScheduleBuildSettings {
                ambiguity_detection: LogLevel::Error,
                ..default()
            });
            // Register the systems in the shuffled order, with the explicit
            // ordering contract attached to the systems themselves:
            // spawn_block_labels -> toggle_debug_viz -> draw_debug_viz and
            // walk_and_collide -> update_chunk_streaming -> draw_debug_viz.
            for index in order {
                match index {
                    0 => schedule.add_systems(walk_and_collide.in_set(PlayerSystems::Move)),
                    1 => schedule.add_systems(
                        update_chunk_streaming
                            .in_set(WorldSystems::Streaming)
                            .after(PlayerSystems::Move),
                    ),
                    2 => schedule.add_systems(spawn_block_labels),
                    3 => schedule.add_systems(toggle_debug_viz.after(spawn_block_labels)),
                    4 => schedule.add_systems(
                        draw_debug_viz
                            .after(toggle_debug_viz)
                            .after(WorldSystems::Streaming),
                    ),
                    _ => unreachable!(),
                };
            }
            let mut world = World::new();
            world.init_resource::<ButtonInput<KeyCode>>();
            world.init_resource::<DebugVizEnabled>();
            if let Err(err) = schedule.initialize(&mut world) {
                panic!("seed {seed}: schedule order {order:?} failed to build: {err:?}");
            }
        }
    }
}
