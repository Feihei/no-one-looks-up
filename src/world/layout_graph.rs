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
/// A maximal straight street run: start vertex, end vertex, average width,
/// road tier (ADR-0009: the arterial cross carries `EdgeKind::Arterial`).
type StreetRun = (Vertex, Vertex, f32, EdgeKind);

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
    /// Straight mid-run vertex kept as an edge anchor (R3.20): every unit
    /// edge endpoint hosts a node, so per-unit edge geometry survives in the
    /// graph even where no street crosses. Never a spawn point.
    Pass,
    /// Center of a [`BlockType::Plaza`] block.
    Plaza,
    /// Mouth of a tier-1 lane on a tier-0 street: the T junction ADR-0008
    /// exists to produce. One street edge passes through it and one lane
    /// terminates on it (degree 3), at walking scale (10-46 m apart) instead
    /// of the merged-block scale (100-250 m).
    LaneMouth,
}

/// A point of the street network.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayoutNode {
    /// World position (meters; grid vertices sit on the 50 m lattice).
    pub pos: Vec2,
    pub kind: NodeKind,
}

/// Road tier of a layout edge (ADR-0008 two-tier network, ADR-0009 arterial).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EdgeKind {
    /// Tier 0: the street between blocks (the pre-ADR-0008 behaviour).
    /// The default so edges serialized before the field existed deserialize
    /// as streets, exactly what they were.
    #[default]
    Street,
    /// Tier 0 arterial (ADR-0009): the cross that splits the map into four
    /// quadrants. Carries its own drawn width (10-15 m) instead of the
    /// flanking blocks' average, reads wider on the minimap/debug view, and
    /// behaves as a street everywhere else (buildings face it, lane mouths
    /// end on it, walking traffic crosses it).
    Arterial,
    /// Tier 1: a lane inside one block, cut by the sub-block pass. Runs the
    /// full depth of its block and ends on tier-0 streets (T junctions).
    Lane,
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
    /// Road tier (ADR-0008). Lanes flank the SAME block on both sides (the
    /// parent block's id); placement, furniture and the overhead layer skip
    /// them, the minimap and debug view draw them thinner.
    #[serde(default)]
    pub kind: EdgeKind,
}

/// One lane-facing boundary of a [`SubBlock`] (ADR-0008).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubFrontage {
    /// Boundary start (world coordinates).
    pub a: Vec2,
    /// Boundary end (world coordinates).
    pub b: Vec2,
    /// Distance from the lane centerline to this boundary (= lane half
    /// width): buildings offset from the lane by it, exactly like a street
    /// curb.
    pub lane_half_width: f32,
}

/// A sub-block: one plot cut out of a block's interior by tier-1 lanes
/// (ADR-0008). Plots inside a house row sit flush against each other — the
/// wall-to-wall feel comes from `BuildingParams.spacing`/`setback`, never
/// from gaps between plots.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubBlock {
    /// Parent block id ([`LayoutBlock::id`]).
    pub parent: u32,
    /// Plot polygon, inset to the lane edges (world coordinates, CCW).
    pub polygon: Vec<Vec2>,
    /// Boundaries facing a lane; buildings hug these lines.
    pub frontages: Vec<SubFrontage>,
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
    /// Interior subdivision (ADR-0008); empty means the block is not
    /// subdivided (factory/plaza/forest, or an old save read back). Placement
    /// and collision fall back to `polygon` when this is empty.
    #[serde(default)]
    pub children: Vec<SubBlock>,
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
    /// Effective width (m): average of the flanking blocks' street widths;
    /// the arterial cross carries its own drawn width instead (ADR-0009).
    pub width: f32,
    /// Road tier of this unit edge (ADR-0009): streets and the arterial cross
    /// both arrive here from generation; lanes are appended by subdivision.
    pub kind: EdgeKind,
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
        for (start, end, width, kind) in street_segments(unit_edges) {
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
                kind,
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

/// Merges street unit edges into maximal straight runs and emits one segment
/// per unit edge.
///
/// Returns `(start_vertex, end_vertex, width, kind)` tuples in lattice
/// coordinates; the width is the average over the merged unit edges. A run
/// continues while a collinear unit edge exists — it passes straight THROUGH
/// degree-4 junctions (a north-south street keeps both its halves at a
/// crossroad, even when the crossing street is the arterial), and ends only
/// where the line stops: at the ring, at a T head, or at a corner. Every
/// unit edge along one lattice line carries the same kind (the arterial
/// cross owns its two lines whole, ADR-0009), so the run's kind is read off
/// its first unit edge.
///
/// Per-unit emission is load-bearing (R3.1): a merged run would give
/// placement one anchor mid-street, and since R3.20 every unit endpoint
/// hosts a node, so every unit edge survives into the graph — an earlier
/// node-pair attempt punched 50-100 m holes into streets behind 2+-cell
/// districts (vertices between two cells of one block host no crossing
/// street, so they had no node and their units were dropped).
fn street_segments(unit_edges: &[StreetUnitEdge]) -> Vec<StreetRun> {
    let canonical = |a: Vertex, b: Vertex| {
        if a < b { (a, b) } else { (b, a) }
    };
    // Adjacency: lattice vertex -> (neighbor, width, kind).
    let mut adjacency: HashMap<Vertex, Vec<(Vertex, f32, EdgeKind)>> = HashMap::new();
    for edge in unit_edges {
        let (v0, v1) = edge.vertices;
        adjacency
            .entry(v0)
            .or_default()
            .push((v1, edge.width, edge.kind));
        adjacency
            .entry(v1)
            .or_default()
            .push((v0, edge.width, edge.kind));
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
        for (next, _, _) in adjacency[&start].clone() {
            let key = canonical(start, next);
            if !visited.insert(key) {
                continue;
            }
            let (mut total_width, kind) = edge_info(unit_edges, key);
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
                for (cand, _, _) in adjacency[&head].clone() {
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
                        total_width += edge_info(unit_edges, ckey).0;
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
                for (cand, _, _) in adjacency[&tail].clone() {
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
                        total_width += edge_info(unit_edges, ckey).0;
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
                segments.push((pair[0], pair[1], total_width / count, kind));
            }
        }
    }
    segments
}

/// Looks up the width and kind of a canonicalized unit edge.
fn edge_info(unit_edges: &[StreetUnitEdge], key: ((u32, u32), (u32, u32))) -> (f32, EdgeKind) {
    unit_edges
        .iter()
        .find(|e| canonical_pair(e.vertices) == key)
        .map(|e| (e.width, e.kind))
        .unwrap_or((
            (crate::world::STREET_WIDTH_MIN + crate::world::STREET_WIDTH_MAX) * 0.5,
            EdgeKind::Street,
        ))
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
            children: vec![],
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
                kind: EdgeKind::Street,
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
                children: vec![],
            }],
        };
        let json = serde_json::to_string(&graph).expect("serialize");
        let restored: LayoutGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(graph, restored);
    }

    /// ADR-0008 backward compatibility: a graph serialized before the
    /// subdivision existed (no `children` on blocks, no `kind` on edges)
    /// must deserialize with empty children (= no subdivision, the old
    /// behaviour) and street edges.
    #[test]
    fn pre_subdivision_graph_json_still_loads() {
        let legacy = r#"{
            "bounds": [800.0, 800.0],
            "nodes": [{"pos": [50.0, 50.0], "kind": "Junction"}],
            "edges": [{"from": 0, "to": 0, "width": 8.0,
                       "left_block": 0, "right_block": null}],
            "blocks": [{"id": 0, "kind": "OldCity", "cells": [[1, 1]],
                        "street_width": 8.0, "is_outer": false,
                        "polygon": [[54.0, 54.0], [96.0, 54.0],
                                    [96.0, 96.0], [54.0, 96.0]]}]
        }"#;
        let graph: LayoutGraph = serde_json::from_str(legacy).expect("legacy graph loads");
        assert!(
            graph.blocks[0].children.is_empty(),
            "old save = no subdivision"
        );
        assert_eq!(graph.edges[0].kind, EdgeKind::Street);
        assert_eq!(graph.edges[0].left_block, Some(0));
    }

    /// A subdivided graph (children + lane edges + lane-mouth nodes) must
    /// round-trip bit-exactly through serde.
    #[test]
    fn subdivided_graph_round_trips() {
        let mut graph = LayoutGraph {
            bounds: Vec2::splat(crate::world::WORLD_SIZE),
            ..Default::default()
        };
        graph.blocks.push(LayoutBlock {
            id: 0,
            kind: BlockType::UrbanVillage,
            cells: vec![(1, 1)],
            street_width: 3.0,
            is_outer: false,
            polygon: vec![
                Vec2::new(51.5, 51.5),
                Vec2::new(98.5, 51.5),
                Vec2::new(98.5, 98.5),
                Vec2::new(51.5, 98.5),
            ],
            children: vec![SubBlock {
                parent: 0,
                polygon: vec![
                    Vec2::new(51.5, 51.5),
                    Vec2::new(60.0, 51.5),
                    Vec2::new(60.0, 98.5),
                    Vec2::new(51.5, 98.5),
                ],
                frontages: vec![SubFrontage {
                    a: Vec2::new(60.0, 51.5),
                    b: Vec2::new(60.0, 98.5),
                    lane_half_width: 1.4,
                }],
            }],
        });
        graph.nodes.push(LayoutNode {
            pos: Vec2::new(61.4, 50.0),
            kind: NodeKind::LaneMouth,
        });
        graph.edges.push(LayoutEdge {
            from: 0,
            to: 0,
            width: 2.8,
            left_block: Some(0),
            right_block: Some(0),
            kind: EdgeKind::Lane,
        });
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
            kind: EdgeKind::Street,
        };
        let segments = street_segments(&[unit(0), unit(1), unit(2)]);
        assert_eq!(segments.len(), 3, "one segment per unit edge: {segments:?}");
        for (i, ((r0, c0), (r1, c1), w, kind)) in segments.iter().enumerate() {
            assert_eq!(*kind, EdgeKind::Street);
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
            kind: EdgeKind::Street,
        };
        let unit_h = StreetUnitEdge {
            vertices: ((1, 1), (1, 2)),
            width: 9.0,
            kind: EdgeKind::Street,
        };
        let segments = street_segments(&[unit, unit_h]);
        assert_eq!(segments.len(), 2, "corner splits the run: {segments:?}");
    }
}
