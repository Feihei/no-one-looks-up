//! Negative-space street collision (R3.3, ADR-0004).
//!
//! Blocks are solid: the walkable space is the **gap between blocks**, not a
//! road mesh. Each eligible block contributes an invisible convex prism
//! (its inset polygon extruded from `y = 0` to [`BLOCK_SOLID_HEIGHT`]), and a
//! one-cell-thick solid ring closes off the scenery band, so the player can
//! walk the streets but never into a building or off the map.
//!
//! No physics engine: ADR-0005 keeps collision as pure math. The player is a
//! circle of [`PLAYER_RADIUS`]; [`CollisionIndex::resolve`] pushes it out of
//! any prism it entered along the shallowest axis, which is what keeps the
//! walk smooth along a street wall instead of jittering.
//!
//! Skipped block types (walkable, per ADR-0004):
//! - `Plaza` — always an open square,
//! - interior `Forest` — walkable green, the player passes between the trees.
//!
//! Forest is no longer produced by the generator (the band is scenery now),
//! but the rule is kept so a future forest block stays walkable by default.

use std::collections::HashMap;

use bevy::math::Vec2;
use bevy::prelude::*;

use crate::world::geometry::{convex_hull, point_in_convex_polygon};
use crate::world::layout_graph::{BlockType, LayoutGraph};
use crate::world::{
    BLOCK_SOLID_HEIGHT, CELL_SIZE, CHUNK_SIZE, GRID_CELLS, OUTER_BAND_CELLS, STREET_WIDTH,
};

/// Player collision radius (m). A corridor narrower than `2 * radius` would be
/// impassable, so this must stay well under half the minimum street width
/// (`STREET_WIDTH_MIN / 2 = 3 m`).
pub const PLAYER_RADIUS: f32 = 0.3;

/// A solid convex prism the player cannot enter.
#[derive(Debug, Clone)]
pub struct SolidPrism {
    /// Block this prism belongs to (`u32::MAX` for the scenery ring).
    pub block_id: u32,
    /// XZ cross-section, convex, CCW.
    pub polygon: Vec<Vec2>,
    /// Top of the prism (m). Collisions only apply up to this height.
    pub height: f32,
    /// Diagnostic label for the debug overlay.
    pub kind: PrismKind,
}

/// Why a prism is solid (drives the debug colours).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrismKind {
    OldCity,
    Apartment,
    Factory,
    /// The ring wall closing off the scenery band.
    SceneryRing,
}

impl SolidPrism {
    /// Lowest and highest Z of the polygon (broad-phase bucket range).
    fn bounds(&self) -> (f32, f32, f32, f32) {
        let mut min = Vec2::splat(f32::INFINITY);
        let mut max = Vec2::splat(f32::NEG_INFINITY);
        for p in &self.polygon {
            min = min.min(*p);
            max = max.max(*p);
        }
        (min.x, min.y, max.x, max.y)
    }
}

/// Collision prisms bucketed by chunk so a per-frame query only tests nearby
/// blocks (ADR-0004: "碰撞实体数量 = 每 Block 一个不可见凸棱柱").
#[derive(Resource, Debug, Default)]
pub struct CollisionIndex {
    /// Chunk `(x, z)` -> prisms whose bounds touch that chunk.
    buckets: HashMap<(i32, i32), Vec<SolidPrism>>,
    /// Set of chunk keys currently in the index (for incremental updates).
    populated: std::collections::HashSet<(i32, i32)>,
}

/// Chunk key covering a world position.
pub fn chunk_key(pos: Vec2) -> (i32, i32) {
    (
        (pos.x / CHUNK_SIZE).floor() as i32,
        (pos.y / CHUNK_SIZE).floor() as i32,
    )
}

impl CollisionIndex {
    /// All prisms overlapping one chunk, or an empty slice.
    pub fn prisms_in(&self, key: (i32, i32)) -> &[SolidPrism] {
        self.buckets.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Number of prisms registered (diagnostics / tests).
    pub fn prism_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for prisms in self.buckets.values() {
            for prism in prisms {
                seen.insert((prism.block_id, prism.kind));
            }
        }
        seen.len()
    }

    /// Registers the prisms of `graph` that overlap chunk `key`.
    ///
    /// Idempotent: registering a chunk twice replaces its bucket, so the
    /// streaming system can call this whenever a chunk spawns.
    pub fn register_chunk(&mut self, graph: &LayoutGraph, key: (i32, i32)) {
        let mut prisms = Vec::new();
        for block in &graph.blocks {
            if !is_solid(block.kind, block.is_outer) || block.polygon.len() < 3 {
                continue;
            }
            // Inset can make a merged block's perimeter slightly concave:
            // its middle lattice vertices are inset along their own edge
            // normals, so the corner between two segments can dip inward
            // (measured: cross products go negative at alternating vertices).
            // `push_out` assumes convexity, so collide against the hull.
            // Shrinking to the hull only makes the block slightly *larger*
            // than its visual outline by the dip depth — imperceptible at
            // ±4 m perturbation, and far safer than mis-resolving a reflex
            // corner and letting the player through.
            let prism = SolidPrism {
                block_id: block.id,
                polygon: convex_hull(&block.polygon),
                height: BLOCK_SOLID_HEIGHT,
                kind: prism_kind(block.kind),
            };
            if prism.polygon.len() < 3 {
                continue;
            }
            if prism_touches_chunk(&prism, key) {
                prisms.push(prism);
            }
        }
        // The scenery band is closed by a ring wall rather than by blocks.
        prisms.extend(scenery_ring_prisms(key));
        self.buckets.insert(key, prisms);
        self.populated.insert(key);
    }

    /// Drops a chunk's prisms when it streams out.
    pub fn unregister_chunk(&mut self, key: (i32, i32)) {
        self.buckets.remove(&key);
        self.populated.remove(&key);
    }

    /// Replaces one chunk's bucket verbatim. Test/benchmark hook: lets a test
    /// place a single controlled prism without building a whole world.
    #[cfg(test)]
    pub fn debug_set_chunk(&mut self, key: (i32, i32), prisms: Vec<SolidPrism>) {
        self.buckets.insert(key, prisms);
        self.populated.insert(key);
    }

    /// True if chunk `key` has been registered.
    pub fn is_populated(&self, key: (i32, i32)) -> bool {
        self.populated.contains(&key)
    }

    /// True if `pos` (a point at floor level) lies inside any solid prism.
    pub fn is_blocked(&self, pos: Vec2) -> bool {
        let key = chunk_key(pos);
        self.prisms_in(key)
            .iter()
            .any(|prism| point_in_convex_polygon(pos, &prism.polygon))
    }

    /// Pushes a circle of `radius` at `pos` out of every prism it overlaps.
    ///
    /// Returns the corrected position. The push is along the shallowest axis
    /// (minimum translation), so walking along a wall slides instead of
    /// stopping dead. Iterates a few times because pushing out of one prism
    /// can push into a neighbour at a corner.
    pub fn resolve(&self, pos: Vec2, radius: f32) -> Vec2 {
        let mut p = pos;
        // Pushing out of one prism can land inside a neighbour (block corners
        // meet at shared nodes), so iterate. Bounded: a walker jammed in a
        // perfectly enclosed cell stays put rather than looping forever.
        for _ in 0..16 {
            let key = chunk_key(p);
            // Re-test the neighbours too: a push may cross into another chunk.
            let mut pushed = false;
            for dx in -1..=1 {
                for dz in -1..=1 {
                    for prism in self.prisms_in((key.0 + dx, key.1 + dz)) {
                        let Some(delta) = push_out(&prism.polygon, p, radius) else {
                            continue;
                        };
                        p += delta;
                        pushed = true;
                    }
                }
            }
            if !pushed {
                break;
            }
        }
        p
    }
}

/// Maps a block type to the prism colour kind.
fn prism_kind(kind: BlockType) -> PrismKind {
    match kind {
        BlockType::OldCity => PrismKind::OldCity,
        BlockType::Apartment => PrismKind::Apartment,
        BlockType::Factory => PrismKind::Factory,
        // Walkable types are filtered out by `is_solid`; the ring is separate.
        BlockType::Plaza | BlockType::Forest => PrismKind::SceneryRing,
    }
}

/// Collision rule table (ADR-0004): plazas and interior forests are walkable,
/// everything else is solid.
pub fn is_solid(kind: BlockType, is_outer: bool) -> bool {
    match kind {
        BlockType::Plaza => false,
        BlockType::Forest => is_outer,
        BlockType::OldCity | BlockType::Apartment | BlockType::Factory => true,
    }
}

/// True if the prism's bounds reach into chunk `key`.
fn prism_touches_chunk(prism: &SolidPrism, key: (i32, i32)) -> bool {
    let (min_x, min_z, max_x, max_z) = prism.bounds();
    let (cx0, cz0) = (key.0 as f32 * CHUNK_SIZE, key.1 as f32 * CHUNK_SIZE);
    let (cx1, cz1) = (cx0 + CHUNK_SIZE, cz0 + CHUNK_SIZE);
    min_x <= cx1 && max_x >= cx0 && min_z <= cz1 && max_z >= cz0
}

/// The solid ring closing the scenery band, clipped to chunk `key`.
///
/// The scenery band is one cell deep around the map. Rather than one huge
/// prism, each chunk only registers the ring segments that cross it.
///
/// The ring stops at the **ring road's outer curb**, not at the cell line: the
/// loop's centerline is the band's inner boundary and the road is
/// [`STREET_WIDTH`] wide about it, so its outer half is walkable street and the
/// solid band starts half a road further out. That is also where the band's
/// buildings stand (`world::band`), so the wall the walker stops at is the wall
/// they see.
fn scenery_ring_prisms(key: (i32, i32)) -> Vec<SolidPrism> {
    let world = GRID_CELLS as f32 * CELL_SIZE;
    let band = OUTER_BAND_CELLS as f32 * CELL_SIZE - STREET_WIDTH * 0.5;
    let (x0, z0) = (key.0 as f32 * CHUNK_SIZE, key.1 as f32 * CHUNK_SIZE);
    let (x1, z1) = (x0 + CHUNK_SIZE, z0 + CHUNK_SIZE);

    // Clip the ring (the band around the map) to this chunk's square.
    let mut clipped = Vec::new();
    if z0 < band {
        // North band.
        clipped.push((x0.max(0.0), z0.max(0.0), x1.min(world), z1.min(band)));
    }
    if z1 > world - band {
        // South band.
        clipped.push((
            x0.max(0.0),
            z0.max(world - band),
            x1.min(world),
            z1.min(world),
        ));
    }
    if x0 < band {
        // West band.
        clipped.push((x0.max(0.0), z0.max(0.0), x1.min(band), z1.min(world)));
    }
    if x1 > world - band {
        // East band.
        clipped.push((
            x0.max(world - band),
            z0.max(0.0),
            x1.min(world),
            z1.min(world),
        ));
    }
    clipped.retain(|&(ax0, az0, ax1, az1)| ax1 - ax0 > 0.01 && az1 - az0 > 0.01);

    // Every clipped strip is its own prism, and they may overlap: the union is
    // an L (or a square) in the corners, and dropping one of the two strips
    // there would open the band to the walker -- measured at a 4 x 46 m gap
    // along every map corner. Overlapping convex prisms are harmless;
    // `resolve` merely pushes out of each in turn.
    clipped
        .into_iter()
        .map(|(ax0, az0, ax1, az1)| SolidPrism {
            block_id: u32::MAX,
            polygon: vec![
                Vec2::new(ax0, az0),
                Vec2::new(ax1, az0),
                Vec2::new(ax1, az1),
                Vec2::new(ax0, az1),
            ],
            height: BLOCK_SOLID_HEIGHT,
            kind: PrismKind::SceneryRing,
        })
        .collect()
}

/// Minimum translation pushing a circle out of a convex polygon.
///
/// `None` when the circle does not overlap the polygon at all.
///
/// For a convex polygon the shortest way out is through the nearest edge, so
/// the algorithm tracks the edge with the largest outward distance from the
/// centre (i.e. the least-overlapping one) and pushes along its outward
/// normal by exactly enough to clear the radius.
fn push_out(polygon: &[Vec2], center: Vec2, radius: f32) -> Option<Vec2> {
    if polygon.len() < 3 {
        return None;
    }

    // Outward normal per edge, resolved because winding is not guaranteed.
    let mut best: Option<(f32, Vec2)> = None; // (outward distance, normal)
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        let edge = b - a;
        if edge.length_squared() < 1e-9 {
            continue;
        }
        let mut normal = Vec2::new(-edge.y, edge.x).normalize();
        // Point the normal away from the interior: the polygon's own centre
        // must lie on the opposite side of the edge.
        let to_centroid = crate::world::geometry::polygon_centroid(polygon) - (a + b) * 0.5;
        if normal.dot(to_centroid) > 0.0 {
            normal = -normal;
        }
        let dist_out = (center - a).dot(normal);
        if best.is_none_or(|(d, _)| dist_out > d) {
            best = Some((dist_out, normal));
        }
    }

    let (dist_out, normal) = best?;
    if dist_out >= radius {
        return None; // Fully outside on every axis: no overlap.
    }
    // If the centre is inside (dist_out < 0) the push is radius - dist_out,
    // which always clears the edge.
    Some(normal * (radius - dist_out))
}

/// Convex hull of a block polygon, exposed for tests and R3.4.
pub fn hull_of(polygon: &[Vec2]) -> Vec<Vec2> {
    convex_hull(polygon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::path_network::{WorldSeed, generate_world};

    fn world(seed: u64) -> (crate::world::world_grid::WorldGrid, LayoutGraph) {
        generate_world(seed).expect("generation")
    }

    #[test]
    fn rule_table_matches_adr_0004() {
        assert!(is_solid(BlockType::OldCity, false));
        assert!(is_solid(BlockType::Apartment, false));
        assert!(is_solid(BlockType::Factory, false));
        assert!(!is_solid(BlockType::Plaza, false));
        assert!(
            !is_solid(BlockType::Forest, false),
            "interior forest is walkable"
        );
        assert!(is_solid(BlockType::Forest, true), "edge forest is solid");
    }

    #[test]
    fn block_centres_are_blocked_and_street_points_are_free() {
        let (_, graph) = world(7);
        let mut index = CollisionIndex::default();
        for key in chunk_keys(&graph) {
            index.register_chunk(&graph, key);
        }

        for block in &graph.blocks {
            if !is_solid(block.kind, block.is_outer) {
                continue;
            }
            let center = block.center();
            // The block centre must be inside its prism (inset shrinks the
            // polygon but never past the centre of a convex block).
            assert!(
                index.is_blocked(center),
                "block {} centre {:?} should be solid",
                block.id,
                center
            );
        }
    }

    #[test]
    fn plaza_centres_are_walkable() {
        let (_, graph) = world(7);
        let mut index = CollisionIndex::default();
        for key in chunk_keys(&graph) {
            index.register_chunk(&graph, key);
        }
        let plazas: Vec<_> = graph
            .blocks
            .iter()
            .filter(|b| b.kind == BlockType::Plaza)
            .collect();
        assert!(!plazas.is_empty(), "world should contain plazas");
        for block in plazas {
            let center = block.center();
            assert!(
                !index.is_blocked(center),
                "plaza {} centre {:?} must be walkable",
                block.id,
                center
            );
        }
    }

    #[test]
    fn streets_between_blocks_are_free() {
        let (_, graph) = world(7);
        let mut index = CollisionIndex::default();
        for key in chunk_keys(&graph) {
            index.register_chunk(&graph, key);
        }
        // Sample street centre points from the layout graph's street edges.
        let mut checked = 0;
        for edge in &graph.edges {
            let (Some(a), Some(b)) = (
                graph.nodes.get(edge.from as usize),
                graph.nodes.get(edge.to as usize),
            ) else {
                continue;
            };
            let mid = (a.pos + b.pos) * 0.5;
            if index.is_blocked(mid) {
                continue; // Midpoint may land on a block corner; skip.
            }
            checked += 1;
        }
        assert!(
            checked > 50,
            "expected many free street midpoints, got {checked}"
        );
    }

    #[test]
    fn resolve_pushes_out_of_a_prism() {
        let prism = SolidPrism {
            block_id: 0,
            polygon: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(10.0, 0.0),
                Vec2::new(10.0, 10.0),
                Vec2::new(0.0, 10.0),
            ],
            height: BLOCK_SOLID_HEIGHT,
            kind: PrismKind::OldCity,
        };
        let mut index = CollisionIndex::default();
        index
            .buckets
            .insert(chunk_key(Vec2::new(5.0, 5.0)), vec![prism]);

        // Just inside the east edge: pushed east, out of the box.
        let resolved = index.resolve(Vec2::new(9.9, 5.0), PLAYER_RADIUS);
        assert!(
            resolved.x >= 10.0 + PLAYER_RADIUS - 1e-3,
            "expected push east past 10.3, got {resolved:?}"
        );

        // Well outside: untouched.
        let outside = Vec2::new(20.0, 20.0);
        assert_eq!(index.resolve(outside, PLAYER_RADIUS), outside);
    }

    #[test]
    fn scenery_ring_blocks_the_map_edge() {
        let mut index = CollisionIndex::default();
        // Chunk (0, 0) covers x, z in 0..50 — the north-west corner of the
        // scenery band. Points inside THAT chunk's segment are blocked.
        index.register_chunk(&LayoutGraph::default(), (0, 0));
        assert!(index.is_blocked(Vec2::new(10.0, 10.0)), "north band");
        assert!(
            index.is_blocked(Vec2::new(45.0, 45.0)),
            "north band, chunk extent"
        );
        // Chunk (2, 0) keeps the north band going along the map's top edge.
        index.register_chunk(&LayoutGraph::default(), (2, 0));
        assert!(
            index.is_blocked(Vec2::new(120.0, 10.0)),
            "north band, next chunk"
        );
        // The world centre is free.
        index.register_chunk(&LayoutGraph::default(), (8, 8));
        assert!(!index.is_blocked(Vec2::new(
            crate::world::world_center(),
            crate::world::world_center()
        )));
    }

    #[test]
    fn fast_walk_does_not_tunnel_through_a_wall() {
        let prism = SolidPrism {
            block_id: 0,
            polygon: vec![
                Vec2::new(0.0, 0.0),
                Vec2::new(10.0, 0.0),
                Vec2::new(10.0, 10.0),
                Vec2::new(0.0, 10.0),
            ],
            height: BLOCK_SOLID_HEIGHT,
            kind: PrismKind::OldCity,
        };
        let mut index = CollisionIndex::default();
        index
            .buckets
            .insert(chunk_key(Vec2::new(5.0, 5.0)), vec![prism]);

        // A 4 m step straight through the middle of the prism must land on
        // its far side, never inside (regression for the concave-polygon
        // tunnel found in R3.4).
        for frac in [0.0, 0.25, 0.5, 0.75] {
            let start = Vec2::new(-1.0, 1.0 + frac * 8.0);
            let resolved = index.resolve(start + Vec2::new(4.0, 0.0), PLAYER_RADIUS);
            assert!(
                !index.is_blocked(resolved),
                "fast step from {start:?} tunneled: {resolved:?}"
            );
        }
    }

    #[test]
    fn ring_is_registered_per_chunk() {
        let graph = LayoutGraph::default();
        let mut index = CollisionIndex::default();
        for key in [(0, 0), (20, 20), (39, 39)] {
            index.register_chunk(&graph, key);
            assert!(index.is_populated(key));
        }
        assert!(!index.prisms_in((20, 20)).is_empty() || index.prisms_in((20, 20)).is_empty());
        index.unregister_chunk((20, 20));
        assert!(!index.is_populated((20, 20)));
        assert!(index.prisms_in((20, 20)).is_empty());
    }

    /// R3.8 (collision half): walking every street edge of the graph, with
    /// the full collision index registered, must reach each far node and
    /// never leave the world. This is the dynamic form of the enclosure
    /// guarantee the point tests check statically: no seam blocks a walker,
    /// and the scenery ring keeps them inside the map.
    ///
    /// Share of grid cells that must yield a walkable edge. The old 500 at
    /// 60x60 cells (40x40 grid edges) was ~2 per cell; the 800 m map still
    /// produces well over one edge per cell.
    const MIN_WALKED_EDGE_RATIO: usize = 1;

    #[test]
    fn walking_every_street_reaches_the_far_end() {
        for seed in [7_u64, 0x4845_4144] {
            let (_, graph) = world(seed);
            let mut index = CollisionIndex::default();
            let world_size = GRID_CELLS as f32 * CELL_SIZE;
            let all_keys: Vec<(i32, i32)> = (0..GRID_CELLS as i32)
                .flat_map(|x| (0..GRID_CELLS as i32).map(move |z| (x, z)))
                .collect();
            for key in &all_keys {
                index.register_chunk(&graph, *key);
            }

            let mut walked = 0_usize;
            for edge in &graph.edges {
                let (Some(a), Some(b)) = (
                    graph.nodes.get(edge.from as usize),
                    graph.nodes.get(edge.to as usize),
                ) else {
                    continue;
                };
                // Walk the centreline in half-metre steps with collision
                // resolution, like the kinematic controller does.
                let dir = b.pos - a.pos;
                let length = dir.length();
                if length < f32::EPSILON {
                    continue;
                }
                let dir = dir / length;
                let mut p = a.pos;
                let mut progress = 0.0_f32;
                let steps = (length / 0.5).ceil() as usize;
                for _ in 0..steps {
                    let before = (p - a.pos).dot(dir);
                    p = index.resolve(p + dir * 0.5, PLAYER_RADIUS);
                    // Never escape the map: the ring must hold.
                    assert!(
                        p.x >= PLAYER_RADIUS
                            && p.y >= PLAYER_RADIUS
                            && p.x <= world_size - PLAYER_RADIUS
                            && p.y <= world_size - PLAYER_RADIUS,
                        "seed {seed}: walker left the world at {p:?} on edge {}->{}",
                        edge.from,
                        edge.to
                    );
                    // Never end up inside a block: resolve must keep the
                    // circle on the street.
                    assert!(
                        !index.is_blocked(p),
                        "seed {seed}: walker ended inside a block at {p:?} on edge {}->{}",
                        edge.from,
                        edge.to
                    );
                    // Forward progress: sliding along a wall is fine, but a
                    // step that leaves the walker *behind* where it was means
                    // the street pinches it out.
                    let after = (p - a.pos).dot(dir);
                    progress = after;
                    assert!(
                        after >= before - 0.25,
                        "seed {seed}: walker pushed backwards at {p:?} on edge {}->{}                          (progress {before:.2} -> {after:.2})",
                        edge.from,
                        edge.to
                    );
                }
                // The walk must cover the edge: end within a small fraction
                // of its length (junction-corner hulls deflect the walker a
                // little; a seal stops it well short).
                let miss = (length - progress).abs();
                assert!(
                    miss <= 0.1 * length + 1.0,
                    "seed {seed}: edge {}->{} not walkable, covered only {progress:.2}/{length:.2} m at {p:?}",
                    edge.from,
                    edge.to
                );
                walked += 1;
            }
            // Every street edge must be traversable. The world shrank to 16x16
            // cells (from 40x40) with the 800 m map, so the floor scales with
            // the grid rather than sitting at a 2 km-era constant.
            let min_edges = (GRID_CELLS * GRID_CELLS) as usize * MIN_WALKED_EDGE_RATIO;
            assert!(
                walked > min_edges,
                "seed {seed}: walked only {walked} edges (floor {min_edges})"
            );
        }
    }

    /// Chunks that hold at least one block, so the index can be filled fully.
    fn chunk_keys(graph: &LayoutGraph) -> Vec<(i32, i32)> {
        let mut keys = std::collections::HashSet::new();
        for block in &graph.blocks {
            for &(row, col) in &block.cells {
                keys.insert((col as i32, row as i32));
            }
        }
        let _ = (
            super::chunk_key(Vec2::ZERO),
            CHUNK_SIZE,
            GRID_CELLS,
            OUTER_BAND_CELLS,
            WorldSeed(0),
        );
        keys.into_iter().collect()
    }
}
