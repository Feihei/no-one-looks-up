//! Scenery band: the ring of cells between the ring road and the map edge.
//!
//! The outermost cell ring is not a block (dev-plan boundary decision,
//! 2026-10-21): it owns no street, no facade and no collision prism of its own,
//! because a 50 m band cannot absorb the perturbed blocks next to it. What it
//! *does* own is the view. Standing on the ring road, the band is what the
//! walker faces across the street -- and an empty band is a flat dark plane
//! running to the map edge with the hill silhouettes floating above it.
//!
//! So the band is dressed, not built:
//!
//! - a **row of building masses** along the band's inner edge on all four
//!   sides, their facades hugging the line the collision ring already seals
//!   ([`BAND_INNER`]). That is the one honest thing about this row: the wall
//!   the walker stops at and the wall the walker sees are the same wall, 0.4 to
//!   1.6 m of setback apart;
//! - **trees in the alleys** the row leaves between shops, and a small **grove
//!   in each map corner** -- the corner of the band that neither row reaches.
//!
//! The masses are ordinary buildings rather than a separate silhouette system:
//! same cube, same facade materials, same roofs, so the ring reads as the same
//! city continuing instead of a painted backdrop. They are planned as
//! [`PlacedKind::SceneryBuilding`] because they are the one kind of building
//! that legitimately stands *inside* a street corridor (the ring road's outer
//! half is sealed solid, so there is no walkable space there to invade).
//!
//! # Ownership, determinism
//!
//! Each side generates its whole row from its own stream and the caller keeps
//! the objects whose anchor lands in its chunk -- the same rule every other
//! placement pass uses, so nothing is planned twice and nothing depends on load
//! order. Salts 9000+ are the band's own; no existing sequence moves.

use bevy::math::{Vec2, Vec3};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::world::facade::{FacadeStyle, snap_to_bay};
use crate::world::layout_graph::BlockType;
use crate::world::placement::{
    self, ChunkPlan, PlacedKind, PlacedObject, TREE_SCALE_MAX, TREE_SCALE_MIN, TreeInstance,
    anchor_chunk,
};
use crate::world::{CELL_SIZE, OUTER_BAND_CELLS, STREET_WIDTH, WORLD_SIZE};

/// Salt of the band's per-side streams. Free range: buildings use 0/16/32...,
/// street trees 1000, street furniture 12000, the overhead layer 13000.
const BAND_SALT: u32 = 9000;

/// Inner edge of the band (m): the ring road's **outer curb**.
///
/// The loop's centerline is the band cell line (exact on every side, the ring
/// lattice lines are never perturbed) and the road is [`STREET_WIDTH`] wide
/// about it, so the band starts half a road outside that line. The collision
/// ring uses the same number (`collision::scenery_ring_prisms`), which is what
/// makes the wall the walker stops at the wall they see.
pub const BAND_INNER: f32 = OUTER_BAND_CELLS as f32 * CELL_SIZE - STREET_WIDTH * 0.5;

/// Facade setback behind [`BAND_INNER`] (m). Small on purpose: the collision
/// ring starts at `BAND_INNER`, so the walker's invisible wall sits 0.4-1.6 m
/// in front of the visible facade rather than metres in front of nothing.
const SETBACK: (f32, f32) = (0.4, 1.6);

/// Frontage along the row (m), before the bay snap.
const FRONTAGE: (f32, f32) = (5.0, 13.0);

/// Depth away from the street (m). The band is 50 m deep, so even the deepest
/// mass leaves 30 m of empty band behind it -- never seen, the row is opaque.
const DEPTH: (f32, f32) = (7.0, 14.0);

/// Share of row slots left as an alley instead of a building.
const ALLEY_CHANCE: f32 = 0.13;

/// Width (m) and depth (m) of the alleys the row leaves.
const ALLEY_WIDTH: (f32, f32) = (2.5, 5.0);
const ALLEY_DEPTH: f32 = 9.0;

/// Gap kept from the neighbouring row's corner (m). The two rows must not
/// interpenetrate where they meet: this row's first footprint starts here,
/// and the other row's footprints end `SETBACK.0` short of [`BAND_INNER`], so
/// the two can never touch.
const CORNER: f32 = BAND_INNER + 1.0;

/// Trees in each corner grove (a 3 x 3 jittered grid).
const GROVE_TREES: usize = 9;

/// The four sides of the band, in a fixed order (the RNG streams are per
/// side, so the order is part of the contract).
struct Side {
    /// Boundary point at `along == 0`.
    base: Vec2,
    /// Unit direction the row runs in.
    axis: Vec2,
    /// Unit direction out of the street, into the band.
    out: Vec2,
}

impl Side {
    /// A point `along` the row and `depth` into the band.
    fn at(&self, along: f32, depth: f32) -> Vec2 {
        self.base + self.axis * along + self.out * depth
    }

    /// Yaw that puts a building's frontage (`size.x`) along the row.
    fn yaw(&self) -> f32 {
        (-self.axis.y).atan2(self.axis.x)
    }
}

const SIDES: [Side; 4] = [
    // North: the row runs west-to-east, the band is to the north (-Z).
    Side {
        base: Vec2::new(0.0, BAND_INNER),
        axis: Vec2::X,
        out: Vec2::NEG_Y,
    },
    Side {
        base: Vec2::new(0.0, WORLD_SIZE - BAND_INNER),
        axis: Vec2::X,
        out: Vec2::Y,
    },
    Side {
        base: Vec2::new(BAND_INNER, 0.0),
        axis: Vec2::Y,
        out: Vec2::NEG_X,
    },
    Side {
        base: Vec2::new(WORLD_SIZE - BAND_INNER, 0.0),
        axis: Vec2::Y,
        out: Vec2::X,
    },
];

/// Plans the band content anchored in `chunk`.
///
/// Cheap enough to run for every chunk: the four rows are ~180 candidates in
/// total, pure arithmetic, and the filter drops all but the handful that belong
/// here.
pub fn plan(plan: &mut ChunkPlan, seed: u64, chunk: (i32, i32), tree_assets: u8) {
    for (index, side) in SIDES.iter().enumerate() {
        plan_row(plan, seed, index, side, chunk, tree_assets);
    }
    for corner in 0..4 {
        plan_corner_grove(plan, seed, corner, chunk, tree_assets);
    }
}

/// One side's row: shops of mixed height with alleys between them.
fn plan_row(
    plan: &mut ChunkPlan,
    seed: u64,
    index: usize,
    side: &Side,
    chunk: (i32, i32),
    tree_assets: u8,
) {
    let mut rng = band_rng(seed, index as u32);
    let end = WORLD_SIZE - CORNER;
    let yaw = side.yaw();
    let mut along = CORNER;

    while along < end {
        // The alley roll is spent whether or not a building follows, so the
        // stream does not depend on where the row happens to be cut.
        let alley = rng.random::<f32>() < ALLEY_CHANCE;
        if alley {
            let width = rng.random_range(ALLEY_WIDTH.0..=ALLEY_WIDTH.1);
            let count = rng.random_range(1..=2u32);
            for _ in 0..count {
                let at = along + rng.random_range(0.0..=width);
                let depth = rng.random_range(1.2..=ALLEY_DEPTH);
                let tree = tree_instance(&mut rng, tree_assets);
                if let Some(tree) = tree {
                    let position = side.at(at, depth);
                    if anchor_chunk(position) == chunk {
                        push_scenery_tree(plan, position, tree);
                    }
                }
            }
            along += width + rng.random_range(0.0..=0.4);
            continue;
        }

        // Draw the whole building before deciding whether it fits: the row is
        // a single stream, and a skip must not shift what comes after it.
        let kind = roll_style(&mut rng);
        let dims = FacadeStyle::of(kind)
            .expect("band styles are built types")
            .dims();
        let frontage = snap_to_bay(rng.random_range(FRONTAGE.0..=FRONTAGE.1), dims.bay);
        let depth = rng.random_range(DEPTH.0..=DEPTH.1);
        let setback = rng.random_range(SETBACK.0..=SETBACK.1);
        let (height_min, height_max) = height_range(kind);
        let height = rng.random_range(height_min..=height_max);
        let wear = rng.random_range(0.0..=1.0);

        if along + frontage > end {
            break;
        }
        let position = side.at(along + frontage * 0.5, setback + depth * 0.5);
        along += frontage + rng.random_range(0.0..=0.5);

        if anchor_chunk(position) != chunk {
            continue;
        }
        // Height snaps to whole floors for the same reason as a block
        // building: the facade shader counts floors down from the roofline.
        let floors = (height / dims.floor).round().max(1.0);
        plan.objects.push(PlacedObject {
            kind: PlacedKind::SceneryBuilding(kind),
            position,
            yaw,
            size: Vec3::new(frontage, floors * dims.floor, depth),
            footprint_half: Vec2::new(frontage, depth) * 0.5,
            wear,
            variant: placement::facade_variant(position),
            tree: None,
            base: 0.0,
        });
    }
}

/// The band corner neither row reaches: a 50 x 50 m patch of empty band. From
/// the ring road's corner it is the notch between the two rows, so it gets a
/// grove instead of a bare plane running to the map edge.
fn plan_corner_grove(
    plan: &mut ChunkPlan,
    seed: u64,
    corner: usize,
    chunk: (i32, i32),
    tree_assets: u8,
) {
    if tree_assets == 0 {
        return;
    }
    let mut rng = band_rng(seed, 100 + corner as u32);
    // The grove sits in the half of the patch nearest the street corner.
    let (near, far) = (BAND_INNER * 0.36, BAND_INNER * 0.92);
    let (x0, z0) = match corner {
        0 => (0.0, 0.0),
        1 => (WORLD_SIZE, 0.0),
        2 => (0.0, WORLD_SIZE),
        _ => (WORLD_SIZE, WORLD_SIZE),
    };
    let sign_x = if x0 > 0.0 { -1.0 } else { 1.0 };
    let sign_z = if z0 > 0.0 { -1.0 } else { 1.0 };
    let cells = (GROVE_TREES as f32).sqrt() as usize;
    for row in 0..cells {
        for col in 0..cells {
            let jitter = 0.32;
            let fx = (col as f32 + 0.5) / cells as f32 + rng.random_range(-jitter..=jitter);
            let fz = (row as f32 + 0.5) / cells as f32 + rng.random_range(-jitter..=jitter);
            let across = near + (far - near) * fx.clamp(0.0, 1.0);
            let along = near + (far - near) * fz.clamp(0.0, 1.0);
            let position = Vec2::new(x0 + sign_x * across, z0 + sign_z * along);
            let Some(tree) = tree_instance(&mut rng, tree_assets) else {
                return;
            };
            if anchor_chunk(position) == chunk {
                push_scenery_tree(plan, position, tree);
            }
        }
    }
}

/// Style mix along the row: mostly shophouses, a few slabs, the odd shed --
/// the 50/30/20 look of the city, sampled per building rather than per block
/// (a row of 25 m apartment slabs would read as a wall, not a street).
///
/// Weighted further towards shophouses than the city itself: the row is seen
/// from 3-5 m across the ring road, and an 24 m slab at that distance is a
/// cliff rather than a skyline.
fn roll_style(rng: &mut StdRng) -> BlockType {
    let roll: f32 = rng.random();
    if roll < 0.72 {
        BlockType::OldCity
    } else if roll < 0.88 {
        BlockType::Apartment
    } else {
        BlockType::Factory
    }
}

/// Height range (m) per style. Taller than the corresponding block table:
/// these masses are seen from 1-2 m away at eye height, and a 3-6 m shophouse
/// row would put the roofline under the walker's chin.
fn height_range(kind: BlockType) -> (f32, f32) {
    match kind {
        BlockType::Apartment => (12.0, 21.0),
        BlockType::Factory => (9.0, 15.0),
        _ => (6.0, 12.0),
    }
}

fn tree_instance(rng: &mut StdRng, tree_assets: u8) -> Option<TreeInstance> {
    // The three draws happen whether or not a tree follows, so a plan made
    // while the tree meshes were still missing re-plans into the same row.
    let asset = rng.random_range(0..tree_assets.max(1));
    let scale = rng.random_range(TREE_SCALE_MIN..=TREE_SCALE_MAX);
    let yaw = rng.random_range(0.0..std::f32::consts::TAU);
    (tree_assets > 0).then_some(TreeInstance { asset, scale, yaw })
}

fn push_scenery_tree(plan: &mut ChunkPlan, position: Vec2, tree: TreeInstance) {
    plan.objects.push(PlacedObject {
        kind: PlacedKind::SceneryTree,
        position,
        yaw: tree.yaw,
        size: Vec3::ZERO,
        footprint_half: Vec2::ZERO,
        wear: 0.0,
        variant: 0,
        tree: Some(tree),
        base: 0.0,
    });
}

/// Per-side stream (the placement module's `hash_seed` recipe, at its own
/// salt).
fn band_rng(seed: u64, side: u32) -> StdRng {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29)
        ^ (side as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (BAND_SALT as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    StdRng::seed_from_u64(mixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::CHUNK_SIZE;

    /// Every chunk that touches the band ring. All band objects anchor inside
    /// it, which is the point of the ring being exactly one cell deep.
    fn band_chunks() -> Vec<(i32, i32)> {
        let last = (WORLD_SIZE / CHUNK_SIZE) as i32 - 1;
        let mut out = Vec::new();
        for x in 0..=last {
            for z in 0..=last {
                if x == 0 || z == 0 || x == last || z == last {
                    out.push((x, z));
                }
            }
        }
        out
    }

    fn rows(seed: u64) -> Vec<PlacedObject> {
        let mut chunk_plan = ChunkPlan::default();
        let mut all = Vec::new();
        for chunk in band_chunks() {
            chunk_plan.objects.clear();
            plan(&mut chunk_plan, seed, chunk, 3);
            all.extend(chunk_plan.objects.iter().cloned());
        }
        all
    }

    /// The reason this module exists: the outer side of the ring road carries
    /// buildings on all four sides, and each side's row is dense enough that
    /// the band never reads as an empty plane.
    #[test]
    fn every_side_of_the_ring_gets_a_row() {
        for seed in [1u64, 7, crate::world::chunks::DEFAULT_SEED] {
            let objects = rows(seed);
            let buildings: Vec<&PlacedObject> = objects
                .iter()
                .filter(|o| matches!(o.kind, PlacedKind::SceneryBuilding(_)))
                .collect();
            // Perimeter is 3200 m at ~9 m of frontage: expect a few hundred.
            assert!(
                buildings.len() > 200,
                "seed {seed}: only {} band buildings",
                buildings.len()
            );
            // Count per side: on its band side of the boundary AND past the
            // corner cut, so the corner neighbours are not double counted.
            let on_side = |p: Vec2, side: usize| match side {
                0 => p.y < BAND_INNER && p.x > BAND_INNER,
                1 => p.y > WORLD_SIZE - BAND_INNER && p.x < WORLD_SIZE - BAND_INNER,
                2 => p.x < BAND_INNER && p.y > BAND_INNER,
                _ => p.x > WORLD_SIZE - BAND_INNER && p.y < WORLD_SIZE - BAND_INNER,
            };
            for side in 0..4 {
                let count = buildings
                    .iter()
                    .filter(|b| on_side(b.position, side))
                    .count();
                assert!(
                    count > 40,
                    "seed {seed}: side {side} of the ring has only {count} masses"
                );
            }
            assert!(
                objects
                    .iter()
                    .filter(|o| matches!(o.kind, PlacedKind::SceneryTree))
                    .count()
                    > 40,
                "seed {seed}: the band has no trees"
            );
        }
    }

    /// The row stands in the sealed band: no corner of a mass reaches the
    /// interior, where the walkable streets and the blocks are.
    #[test]
    fn the_row_stands_in_the_sealed_band() {
        let in_band = |p: Vec2| {
            p.x <= BAND_INNER
                || p.x >= WORLD_SIZE - BAND_INNER
                || p.y <= BAND_INNER
                || p.y >= WORLD_SIZE - BAND_INNER
        };
        for seed in [1u64, 7, crate::world::chunks::DEFAULT_SEED] {
            for object in rows(seed) {
                if !matches!(object.kind, PlacedKind::SceneryBuilding(_)) {
                    continue;
                }
                let corners = placement::rotated_corners(
                    object.position,
                    object.yaw,
                    object.size.x,
                    object.size.z,
                );
                for corner in corners {
                    assert!(
                        (-1e-3..=WORLD_SIZE + 1e-3).contains(&corner.x)
                            && (-1e-3..=WORLD_SIZE + 1e-3).contains(&corner.y),
                        "seed {seed}: band corner {corner:?} is off the map"
                    );
                    assert!(
                        in_band(corner),
                        "seed {seed}: band building corner {corner:?} stands in the city, \
                         not in the ring"
                    );
                }
            }
        }
    }

    /// The row must not reach the band's full depth either: a footprint that
    /// ran past the middle of the band would show its back through the chunk
    /// window from the opposite side of the map.
    #[test]
    fn the_row_leaves_the_band_empty_behind_it() {
        for object in rows(crate::world::chunks::DEFAULT_SEED) {
            if !matches!(object.kind, PlacedKind::SceneryBuilding(_)) {
                continue;
            }
            assert!(
                object.size.z + SETBACK.1 < BAND_INNER * 0.5,
                "band building {:.1} m deep leaves no band behind it",
                object.size.z
            );
        }
    }

    /// No two band objects share a spot, and none is planned into two chunks.
    #[test]
    fn band_objects_are_placed_once() {
        for seed in [1u64, 7, crate::world::chunks::DEFAULT_SEED] {
            let mut seen = std::collections::HashSet::new();
            for object in rows(seed) {
                let key = (
                    object.kind,
                    (object.position.x * 4.0).round() as i64,
                    (object.position.y * 4.0).round() as i64,
                );
                assert!(
                    seen.insert(key),
                    "seed {seed}: {:?} is planned twice at {:?}",
                    object.kind,
                    object.position
                );
            }
        }
    }
}
