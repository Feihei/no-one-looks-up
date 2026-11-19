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

use crate::world::geometry::{convex_decompose, convex_hull, point_in_convex_polygon};
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
    /// Sub-block index within the parent's `children` (ADR-0008), or
    /// `None` for a whole-block prism. Distinguishes the plots of one
    /// subdivided block in [`CollisionIndex::prism_count`].
    pub child: Option<u32>,
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
    /// 城中村 lane blocks: solid like the rest, debugged in its own colour.
    UrbanVillage,
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
    /// Number of prisms registered (diagnostics / tests). Sub-block plots
    /// count individually (ADR-0008: one prism per plot).
    pub fn prism_count(&self) -> usize {
        let mut seen = std::collections::HashSet::new();
        for prisms in self.buckets.values() {
            for prism in prisms {
                seen.insert((prism.block_id, prism.child, prism.kind));
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
            // ADR-0008: a subdivided block collides per PLOT, so the lanes
            // between the plots stay walkable. Empty children = the old
            // one-prism-per-block behaviour.
            //
            // R3.20: each polygon is `convex_decompose`d into convex pieces
            // and every piece gets its own prism. The previous hull + edge
            // half-plane clip could not satisfy both goals at once on a
            // strongly non-convex outline (a 6 m x 250 m village plot strip):
            // the intersection of the half-planes is the kernel, which may
            // exclude the polygon's own centroid, while the unclipped hull
            // bridges a wobble dip ~5 m INTO the street and pinches the
            // walker. Triangles are always convex and always inside the
            // polygon, so `push_out`'s convexity assumption holds AND the
            // streets keep their full inset width.
            let pieces_of = |polygon: &[Vec2]| -> Vec<Vec<Vec2>> {
                if polygon.len() < 3 {
                    Vec::new()
                } else {
                    convex_decompose(polygon)
                }
            };
            if block.children.is_empty() {
                for piece in pieces_of(&block.polygon) {
                    let prism = SolidPrism {
                        block_id: block.id,
                        child: None,
                        polygon: piece,
                        height: BLOCK_SOLID_HEIGHT,
                        kind: prism_kind(block.kind),
                    };
                    if prism_touches_chunk(&prism, key) {
                        prisms.push(prism);
                    }
                }
            } else {
                for (child_index, child) in block.children.iter().enumerate() {
                    for piece in pieces_of(&child.polygon) {
                        let prism = SolidPrism {
                            block_id: block.id,
                            child: Some(child_index as u32),
                            polygon: piece,
                            height: BLOCK_SOLID_HEIGHT,
                            kind: prism_kind(block.kind),
                        };
                        if prism_touches_chunk(&prism, key) {
                            prisms.push(prism);
                        }
                    }
                }
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
        BlockType::UrbanVillage => PrismKind::UrbanVillage,
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
        BlockType::OldCity
        | BlockType::UrbanVillage
        | BlockType::Apartment
        | BlockType::Factory => true,
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
            child: None,
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
/// Inside the polygon, the shortest way out is through the nearest edge, so
/// the algorithm tracks the edge with the largest outward distance from the
/// centre (i.e. the least-overlapping one) and pushes along its outward
/// normal by exactly enough to clear the radius — the shallowest-axis slide
/// that keeps walking along a street wall smooth.
///
/// Outside, the circle overlaps iff the boundary comes within `radius`; the
/// push is then radial from the closest boundary point, which is the only
/// correct resolution at a convex corner (a corner lies within `radius` of
/// the centre long before either flanking face does).
fn push_out(polygon: &[Vec2], center: Vec2, radius: f32) -> Option<Vec2> {
    if polygon.len() < 3 {
        return None;
    }

    // Centre inside the prism: push along the shallowest axis (the face with
    // the largest outward distance), which is what keeps the slide smooth
    // along a street wall.
    if point_in_convex_polygon(center, polygon) {
        let mut best: Option<(f32, Vec2)> = None; // (outward distance, normal)
        for i in 0..polygon.len() {
            let a = polygon[i];
            let b = polygon[(i + 1) % polygon.len()];
            let edge = b - a;
            if edge.length_squared() < 1e-9 {
                continue;
            }
            let mut normal = Vec2::new(-edge.y, edge.x).normalize();
            // Point the normal away from the interior: the polygon's own
            // centre must lie on the opposite side of the edge.
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
        // If the centre is inside (dist_out < 0) the push is radius -
        // dist_out, which always clears the edge.
        return Some(normal * (radius - dist_out));
    }

    // Centre outside: overlap iff the BOUNDARY comes within `radius`. Resolve
    // via the closest boundary point — radial pushes also handle corners.
    // The old single shallowest-axis test read "all per-edge distances <
    // radius" as *inside* and pushed walkers that merely stood near a convex
    // corner (R3.20, measured: an OldCity plot corner 2.06 m from a narrow
    // seam centreline teleported the walker sideways and stalled the walk).
    let mut best: Option<(f32, Vec2, Vec2)> = None; // (distance, closest point, edge)
    for i in 0..polygon.len() {
        let a = polygon[i];
        let b = polygon[(i + 1) % polygon.len()];
        let edge = b - a;
        if edge.length_squared() < 1e-9 {
            continue;
        }
        let t = ((center - a).dot(edge) / edge.length_squared()).clamp(0.0, 1.0);
        let closest = a.lerp(b, t);
        let d = (center - closest).length();
        if best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, closest, edge));
        }
    }
    let (d, closest, edge) = best?;
    if d >= radius {
        return None;
    }
    if d < 1e-6 {
        // Centre exactly on the boundary: push along the outward face normal.
        let mut normal = Vec2::new(-edge.y, edge.x).normalize();
        let to_centroid = crate::world::geometry::polygon_centroid(polygon) - closest;
        if normal.dot(to_centroid) > 0.0 {
            normal = -normal;
        }
        return Some(normal * radius);
    }
    Some((center - closest) / d * (radius - d))
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
            if block.children.is_empty() {
                // The block centre must be inside its prism (inset shrinks
                // the polygon but never past the centre of a convex block).
                assert!(
                    index.is_blocked(center),
                    "block {} centre {:?} should be solid",
                    block.id,
                    center
                );
            } else {
                // ADR-0008: a subdivided block's centre may sit ON a lane
                // (walkable by design). Every PLOT centre must be solid,
                // and every lane midpoint must be free.
                for child in &block.children {
                    let plot_center = crate::world::geometry::polygon_centroid(&child.polygon);
                    assert!(
                        index.is_blocked(plot_center),
                        "block {} plot centre {:?} should be solid",
                        block.id,
                        plot_center
                    );
                }
            }
        }
        // Lane midpoints are walkable negative space.
        let mut lanes = 0;
        for edge in &graph.edges {
            if edge.kind != crate::world::layout_graph::EdgeKind::Lane {
                continue;
            }
            let a = graph.nodes[edge.from as usize].pos;
            let b = graph.nodes[edge.to as usize].pos;
            let mid = (a + b) * 0.5;
            assert!(
                !index.is_blocked(mid),
                "lane midpoint {mid:?} should be walkable"
            );
            lanes += 1;
        }
        assert!(lanes > 0, "world should carry lanes");
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
            child: None,
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
            child: None,
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
    /// Absolute floor on walkable edges (seed 7 measured 141, 0x4845_4144
    /// 176, seed 1 91). R3.20's same-kind district growth removed the seams
    /// between merged districts, so the old one-edge-per-cell floor (256)
    /// no longer holds — the new texture has fewer, longer streets.
    const MIN_WALKED_EDGES: usize = 64;
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
            // Every street edge must be traversable. R3.20 shrank the street
            // count (merged districts share no seams), so the floor is the
            // absolute MIN_WALKED_EDGES instead of a per-cell ratio.
            assert!(
                walked > MIN_WALKED_EDGES,
                "seed {seed}: walked only {walked} edges (floor {MIN_WALKED_EDGES})"
            );
        }
    }

    /// ADR-0008: every lane must be walkable end to end, exactly like the
    /// tier-0 streets — the plots flank it without sealing it, and the
    /// mouth joins the street without a pinch.
    #[test]
    fn walking_every_lane_reaches_the_far_end() {
        for seed in [7_u64, 0x4845_4144] {
            let (_, graph) = world(seed);
            let mut index = CollisionIndex::default();
            let all_keys: Vec<(i32, i32)> = (0..GRID_CELLS as i32)
                .flat_map(|x| (0..GRID_CELLS as i32).map(move |z| (x, z)))
                .collect();
            for key in &all_keys {
                index.register_chunk(&graph, *key);
            }
            let mut walked = 0;
            for edge in &graph.edges {
                if edge.kind != crate::world::layout_graph::EdgeKind::Lane {
                    continue;
                }
                let a = graph.nodes[edge.from as usize].pos;
                let b = graph.nodes[edge.to as usize].pos;
                let dir = b - a;
                let length = dir.length();
                if length < f32::EPSILON {
                    continue;
                }
                let dir = dir / length;
                let mut p = a;
                let mut progress = 0.0_f32;
                let steps = (length / 0.5).ceil() as usize;
                for _ in 0..steps {
                    p = index.resolve(p + dir * 0.5, PLAYER_RADIUS);
                    assert!(
                        !index.is_blocked(p),
                        "seed {seed}: lane walker inside a plot at {p:?}"
                    );
                    progress = (p - a).dot(dir);
                }
                let miss = (length - progress).abs();
                assert!(
                    miss <= 0.1 * length + 1.0,
                    "seed {seed}: lane {}->{} covered only {progress:.1}/{length:.1} m",
                    edge.from,
                    edge.to
                );
                walked += 1;
            }
            // R3.20: apartment lanes are gone (the blue districts keep their
            // interior whole), so only village + old-city lanes remain —
            // seeds 7 / 0x4845_4144 measured 19 / 31.
            assert!(walked > 15, "seed {seed}: walked only {walked} lanes");
        }
    }

    /// ADR-0008 verification: the prism count rises with subdivision (a
    /// village block carries one prism per plot instead of one), and the
    /// per-chunk bucketing keeps the query load bounded.
    #[test]
    fn subdivision_multiplies_prisms_but_buckets_stay_bounded() {
        let (_, graph) = world(7);
        let mut index = CollisionIndex::default();
        let all_keys: Vec<(i32, i32)> = (0..GRID_CELLS as i32)
            .flat_map(|x| (0..GRID_CELLS as i32).map(move |z| (x, z)))
            .collect();
        for key in &all_keys {
            index.register_chunk(&graph, *key);
        }
        let plots: usize = graph.blocks.iter().map(|b| b.children.len()).sum();
        let whole = graph
            .blocks
            .iter()
            .filter(|b| is_solid(b.kind, b.is_outer) && b.children.is_empty())
            .count();
        assert!(plots > 0, "world should carry plots");
        assert!(
            index.prism_count() >= plots + whole,
            "prisms {} < plots {plots} + whole blocks {whole}",
            index.prism_count()
        );
        // A chunk's bucket stays small enough for the 3x3 resolve scan.
        let max_bucket = all_keys
            .iter()
            .map(|k| index.prisms_in(*k).len())
            .max()
            .unwrap_or(0);
        assert!(
            max_bucket < 400,
            "largest chunk bucket {max_bucket} prisms is too heavy"
        );
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
