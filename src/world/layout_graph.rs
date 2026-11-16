//! `LayoutGraph`: the compact topological map produced at startup (ADR-0002).
//!
//! The graph holds the street network skeleton (junction/corner nodes and
//! street segments with widths), one entry per block (type label, cells,
//! street width, outer flag, inset polygon), and the world bounds. It is
//! serde-serializable so the R4 save system can rebuild it from the seed
//! (`<1s` recovery requirement); no 3D mesh data lives here.
//!
//! Node kinds (ADR-0002 "节点位置（路口、广场）"): [`NodeKind::Junction`] for
//! street crossings, [`NodeKind::Corner`] for right-angle bends of a street
//! around a block, [`NodeKind::Plaza`] for plaza block centers (placement
//! anchors).

use std::collections::{HashMap, HashSet};

use bevy::ecs::prelude::Resource;
use bevy::math::Vec2;
use serde::{Deserialize, Serialize};

use crate::world::CELL_SIZE;

/// Lattice vertex in grid-line coordinates.
type Vertex = (u32, u32);
/// A maximal straight street run: start vertex, end vertex, average width.
type StreetRun = (Vertex, Vertex, f32);

/// Building style label of a block (ADR-0001: three styles plus open zones).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlockType {
    /// 中式老城区：low-rise, dense (≈50% of blocks).
    OldCity,
    /// 旧公寓楼：mid-rise slabs.
    Apartment,
    /// 旧厂房：tall volumes.
    Factory,
    /// 城中村：3-storey self-built houses packed against 2-3.5 m lanes,
    /// street trees nowhere, clutter everywhere (2026-11 field visit).
    UrbanVillage,
    /// 广场：open plaza, walkable.
    Plaza,
    /// 树林：edge transition zone, dense trees.
    Forest,
}

/// Kind of a layout graph node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// Street crossing (≥3 street edges meet).
    Junction,
    /// Right-angle bend of a street around a block.
    Corner,
    /// Center of a [`BlockType::Plaza`] block.
    Plaza,
}

/// A point of the street network.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutNode {
    /// World position (meters; grid vertices sit on the 50 m lattice).
    pub pos: Vec2,
    pub kind: NodeKind,
}

/// A street segment between two nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutEdge {
    /// Index into [`LayoutGraph::nodes`].
    pub from: u32,
    /// Index into [`LayoutGraph::nodes`].
    pub to: u32,
    /// Effective street width (m): average over the merged unit edges.
    pub width: f32,
    /// Block on the "left" side of `from -> to` (the original's
    /// `edge.left_block`): `None` when that side is the scenery band.
    #[serde(default)]
    pub left_block: Option<u32>,
    /// Block on the "right" side (the original's `edge.right_block`).
    #[serde(default)]
    pub right_block: Option<u32>,
}

/// One block of the world: a merged cell group with a style label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutBlock {
    /// Stable block id (matches `WorldGrid` cell records).
    pub id: u32,
    pub kind: BlockType,
    /// Cells `(row, col)` owned by this block.
    pub cells: Vec<(u32, u32)>,
    /// Random street width of this block (m, `[STREET_WIDTH_MIN, MAX]`);
    /// inset = width / 2 on the block's own territory.
    pub street_width: f32,
    /// True for the outer enclosure band (always solid, ADR-0004).
    pub is_outer: bool,
    /// Block polygon inset by `street_width / 2` (world coordinates, CCW).
    /// Used by the R3 collision prisms and debug visualization.
    pub polygon: Vec<Vec2>,
}

impl LayoutBlock {
    /// Center of the block's cells, in world coordinates.
    pub fn center(&self) -> Vec2 {
        let n = self.cells.len().max(1) as f32;
        self.cells
            .iter()
            .map(|&(row, col)| {
                Vec2::new(
                    (col as f32 + 0.5) * CELL_SIZE,
                    (row as f32 + 0.5) * CELL_SIZE,
                )
            })
            .sum::<Vec2>()
            / n
    }
}

/// A grid vertex hosting a graph node: `row_line`/`col_line` are lattice
/// coordinates (0..=GRID_CELLS), `kind` the node kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodeVertex {
    pub row: u32,
    pub col: u32,
    pub kind: NodeKind,
}

/// One street unit edge between two adjacent cells, in lattice coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StreetUnitEdge {
    /// The two lattice vertices of the unit edge (unordered).
    pub vertices: ((u32, u32), (u32, u32)),
    /// Effective width (m): average of the flanking blocks' street widths.
    pub width: f32,
}

/// The complete world layout: street network + blocks + bounds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, Resource)]
pub struct LayoutGraph {
    /// World extent (meters); generation covers `(0,0)..=bounds`.
    pub bounds: Vec2,
    pub nodes: Vec<LayoutNode>,
    pub edges: Vec<LayoutEdge>,
    pub blocks: Vec<LayoutBlock>,
}

impl LayoutGraph {
    /// Assembles the graph from the generation pass's block records, street
    /// unit edges, and node vertices.
    pub(crate) fn from_generation(
        blocks: Vec<LayoutBlock>,
        unit_edges: &[StreetUnitEdge],
        node_vertices: &[NodeVertex],
        node_positions: &[Vec2],
    ) -> Self {
        let mut graph = LayoutGraph {
            bounds: Vec2::splat(crate::world::WORLD_SIZE),
            blocks,
            ..Default::default()
        };

        // Nodes: deduplicated per grid vertex, plus one plaza node per plaza
        // block (placement anchors; ADR-0002 lists 广场 among node positions).
        //
        // Positions come from the *perturbed* lattice (the same vertices the
        // block polygons inset from), so a street's centerline and its block
        // faces share one line. Using the base lattice here made streets
        // ignore the wobble while blocks followed it: a node perturbed 4 m
        // south put the block face 1 m from the nominal centerline, sealing
        // streets down to a 0.3 m free lane (found by the R3.8 walk test).
        let mut node_index: HashMap<(u32, u32), u32> = HashMap::new();
        for v in node_vertices {
            if node_index
                .insert((v.row, v.col), graph.nodes.len() as u32)
                .is_none()
            {
                graph.nodes.push(LayoutNode {
                    pos: node_positions
                        [v.row as usize * (crate::world::GRID_CELLS as usize + 1) + v.col as usize],
                    kind: v.kind,
                });
            }
        }
        let mut plaza_nodes = Vec::new();
        for block in &graph.blocks {
            if block.kind == BlockType::Plaza {
                plaza_nodes.push(LayoutNode {
                    pos: block.center(),
                    kind: NodeKind::Plaza,
                });
            }
        }
        graph.nodes.extend(plaza_nodes);

        // Street segments: merge collinear unit edges into maximal straight
        // runs and attach the run endpoints to the node map.
        for (start, end, width) in street_segments(unit_edges) {
            let (Some(&from), Some(&to)) = (node_index.get(&start), node_index.get(&end)) else {
                // Maximal run endpoints always host a node (junction, corner,
                // or border-adjacent turn): generation guarantees it. Skip
                // defensively instead of corrupting the graph.
                continue;
            };
            graph.edges.push(LayoutEdge {
                from,
                to,
                width,
                left_block: None,
                right_block: None,
            });
        }
        graph
    }

    /// Inset polygon of a block (computed at generation time).
    pub fn block_polygon(&self, block_id: u32) -> Option<&[Vec2]> {
        self.blocks
            .iter()
            .find(|b| b.id == block_id)
            .map(|b| b.polygon.as_slice())
    }
}

/// Merges street unit edges into maximal straight runs.
///
/// Returns `(start_vertex, end_vertex, width)` triples in lattice
/// coordinates; the width is the average over the merged unit edges. Runs end
/// at map ends or at vertices where the street turns or branches — exactly
/// the vertices that host graph nodes.
fn street_segments(unit_edges: &[StreetUnitEdge]) -> Vec<StreetRun> {
    let canonical = |a: Vertex, b: Vertex| {
        if a < b { (a, b) } else { (b, a) }
    };
    // Adjacency: lattice vertex -> (neighbor, width).
    let mut adjacency: HashMap<Vertex, Vec<(Vertex, f32)>> = HashMap::new();
    for edge in unit_edges {
        let (v0, v1) = edge.vertices;
        adjacency.entry(v0).or_default().push((v1, edge.width));
        adjacency.entry(v1).or_default().push((v0, edge.width));
    }

    // Walk maximal straight runs, marking each unit edge visited once. Runs
    // extend in BOTH directions from the first unvisited edge, so every
    // maximal run is produced exactly once regardless of start order.
    let mut visited: HashSet<(Vertex, Vertex)> = HashSet::new();
    let mut starts: Vec<Vertex> = adjacency.keys().copied().collect();
    starts.sort_unstable();
    let mut segments = Vec::new();
    for start in starts {
        // Collect neighbors to keep `adjacency` borrow-free below.
        for (next, _) in adjacency[&start].clone() {
            let key = canonical(start, next);
            if !visited.insert(key) {
                continue;
            }
            let mut total_width = edge_width(unit_edges, key);
            let mut run = vec![start, next];

            // Extend backward: prepend straight continuations before `start`.
            loop {
                let head = run[0];
                let after = run[1];
                let dir = (
                    after.0 as i64 - head.0 as i64,
                    after.1 as i64 - head.1 as i64,
                );
                let mut continuation = None;
                for (cand, _) in adjacency[&head].clone() {
                    let ckey = canonical(head, cand);
                    if visited.contains(&ckey) {
                        continue;
                    }
                    // Backward continuation: cand sits before `head` on the
                    // same line (cand - head == -dir).
                    if (cand.0 as i64 - head.0 as i64, cand.1 as i64 - head.1 as i64)
                        == (-dir.0, -dir.1)
                    {
                        continuation = Some(cand);
                        break;
                    }
                }
                match continuation {
                    Some(cand) => {
                        let ckey = canonical(head, cand);
                        visited.insert(ckey);
                        total_width += edge_width(unit_edges, ckey);
                        run.insert(0, cand);
                    }
                    None => break,
                }
            }

            // Extend forward past `next`.
            loop {
                let tail = *run.last().expect("run holds vertices");
                let prev = run[run.len() - 2];
                let dir = (tail.0 as i64 - prev.0 as i64, tail.1 as i64 - prev.1 as i64);
                let mut continuation = None;
                for (cand, _) in adjacency[&tail].clone() {
                    let ckey = canonical(tail, cand);
                    if visited.contains(&ckey) {
                        continue;
                    }
                    if (cand.0 as i64 - tail.0 as i64, cand.1 as i64 - tail.1 as i64) == dir {
                        continuation = Some(cand);
                        break;
                    }
                }
                match continuation {
                    Some(cand) => {
                        let ckey = canonical(tail, cand);
                        visited.insert(ckey);
                        total_width += edge_width(unit_edges, ckey);
                        run.push(cand);
                    }
                    None => break,
                }
            }
            let count = (run.len() - 1) as f32;
            // Emit one segment per unit edge (original `_generate_edges_from_grid`
            // does the same). A merged 1900 m run would give placement and chunk
            // ownership a single anchor in the middle of the map, so whole
            // street frontages would never be placed.
            for pair in run.windows(2) {
                segments.push((pair[0], pair[1], total_width / count));
            }
        }
    }
    segments
}

/// Looks up the width of a canonicalized unit edge.
fn edge_width(unit_edges: &[StreetUnitEdge], key: ((u32, u32), (u32, u32))) -> f32 {
    unit_edges
        .iter()
        .find(|e| canonical_pair(e.vertices) == key)
        .map(|e| e.width)
        .unwrap_or((crate::world::STREET_WIDTH_MIN + crate::world::STREET_WIDTH_MAX) * 0.5)
}

fn canonical_pair(a: ((u32, u32), (u32, u32))) -> ((u32, u32), (u32, u32)) {
    if a.0 < a.1 { a } else { (a.1, a.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::geometry;

    fn block(id: u32, kind: BlockType, cells: Vec<(u32, u32)>) -> LayoutBlock {
        LayoutBlock {
            id,
            kind,
            cells,
            street_width: 8.0,
            is_outer: false,
            polygon: vec![],
        }
    }

    #[test]
    fn block_center_is_cell_average() {
        let b = block(0, BlockType::OldCity, vec![(0, 0), (0, 1)]);
        assert_eq!(b.center(), Vec2::new(CELL_SIZE, CELL_SIZE * 0.5));
    }

    #[test]
    fn graph_round_trips_through_serde() {
        let graph = LayoutGraph {
            bounds: Vec2::splat(crate::world::WORLD_SIZE),
            nodes: vec![LayoutNode {
                pos: Vec2::new(50.0, 50.0),
                kind: NodeKind::Junction,
            }],
            edges: vec![LayoutEdge {
                from: 0,
                to: 0,
                width: 8.0,
                left_block: None,
                right_block: None,
            }],
            blocks: vec![LayoutBlock {
                id: 0,
                kind: BlockType::Plaza,
                cells: vec![(1, 1)],
                street_width: 7.0,
                is_outer: false,
                polygon: geometry::inset_polygon_clip(
                    &geometry::cell_rect_polygon(1..2, 1..2),
                    3.5,
                )
                .expect("valid inset"),
            }],
        };
        let json = serde_json::to_string(&graph).expect("serialize");
        let restored: LayoutGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(graph, restored);
    }

    #[test]
    fn collinear_run_emits_one_segment_per_unit() {
        // One vertical street of three unit edges (cells stacked in col 1).
        // Runs stay per-unit (R3.1): a merged 150 m segment would give
        // placement a single anchor at its middle, so the frontages on either
        // side of the seam would never be placed.
        let unit = |row: u32| StreetUnitEdge {
            vertices: ((row, 1), (row + 1, 1)),
            width: 8.0,
        };
        let segments = street_segments(&[unit(0), unit(1), unit(2)]);
        assert_eq!(segments.len(), 3, "one segment per unit edge: {segments:?}");
        for (i, ((r0, c0), (r1, c1), w)) in segments.iter().enumerate() {
            assert_eq!((*r0, *c0), (i as u32, 1));
            assert_eq!((*r1, *c1), (i as u32 + 1, 1));
            assert_eq!(*w, 8.0);
        }
    }

    #[test]
    fn perpendicular_run_does_not_merge_through_a_corner() {
        // An L-shaped street: two unit edges meeting at a right angle.
        let unit = StreetUnitEdge {
            vertices: ((0, 1), (1, 1)),
            width: 7.0,
        };
        let unit_h = StreetUnitEdge {
            vertices: ((1, 1), (1, 2)),
            width: 9.0,
        };
        let segments = street_segments(&[unit, unit_h]);
        assert_eq!(segments.len(), 2, "corner splits the run: {segments:?}");
    }
}
