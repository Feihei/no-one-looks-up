//! ADR-0008 verification metrics: prints the two-tier road network's
//! walking-scale statistics for a set of seeds (lane counts, T-junction
//! density, node-gap distribution, prism counts, generation time).
//!
//! ```text
//! cargo run --example subdiv_stats
//! ```
use std::time::Instant;

use no_one_looks_up::world::collision::CollisionIndex;
use no_one_looks_up::world::geometry::{point_in_convex_inclusive, polygon_area};
use no_one_looks_up::world::layout_graph::{BlockType, EdgeKind, NodeKind};
use no_one_looks_up::world::path_network::generate_world;
use no_one_looks_up::world::placement::{PlacedKind, plan_chunk};
use no_one_looks_up::world::{GRID_CELLS, VILLAGE_STREET_WIDTH_MAX, VILLAGE_STREET_WIDTH_MIN};

fn main() {
    for seed in [0x1u64, 0x7, 0x4845_4144] {
        let t0 = Instant::now();
        let (_, graph) = generate_world(seed).expect("generation");
        let gen_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let lanes = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Lane)
            .count();
        let streets = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Street)
            .count();
        let mouths = graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::LaneMouth)
            .count();
        let degree3 = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                let i = *i as u32;
                graph
                    .edges
                    .iter()
                    .filter(|e| e.from == i || e.to == i)
                    .count()
                    == 3
            })
            .count();

        // Node-gap distribution along street edges (the walking-scale
        // metric: gaps of 10-24 m are the ADR's target band).
        let mut band_10_24 = 0;
        let mut band_24_50 = 0;
        let mut over_50 = 0;
        for edge in &graph.edges {
            if edge.kind != EdgeKind::Street {
                continue;
            }
            let a = graph.nodes[edge.from as usize].pos;
            let b = graph.nodes[edge.to as usize].pos;
            let len = a.distance(b);
            if (10.0..24.0).contains(&len) {
                band_10_24 += 1;
            } else if (24.0..50.0).contains(&len) {
                band_24_50 += 1;
            } else if len >= 50.0 {
                over_50 += 1;
            }
        }

        // Lane width / pitch sanity.
        let (mut wmin, mut wmax) = (f32::MAX, f32::MIN);
        for edge in &graph.edges {
            if edge.kind == EdgeKind::Lane {
                wmin = wmin.min(edge.width);
                wmax = wmax.max(edge.width);
            }
        }

        // Plots and prisms.
        let plots: usize = graph.blocks.iter().map(|b| b.children.len()).sum();
        let village_plots: Vec<usize> = graph
            .blocks
            .iter()
            .filter(|b| b.kind == BlockType::UrbanVillage)
            .map(|b| b.children.len())
            .collect();
        let mut index = CollisionIndex::default();
        for cx in 0..GRID_CELLS as i32 {
            for cz in 0..GRID_CELLS as i32 {
                index.register_chunk(&graph, (cx, cz));
            }
        }

        println!(
            "seed {seed:#x}: gen {gen_ms:.0} ms | blocks {} | streets {streets} lanes {lanes} | \
             mouths {mouths} degree3 {degree3} ({:.0}% of {} nodes)",
            graph.blocks.len(),
            degree3 as f32 / graph.nodes.len() as f32 * 100.0,
            graph.nodes.len()
        );
        println!(
            "  street node gaps: 10-24 m {band_10_24} | 24-50 m {band_24_50} | >=50 m {over_50}"
        );
        println!(
            "  lane width {wmin:.2}-{wmax:.2} m (village streets {VILLAGE_STREET_WIDTH_MIN}-{VILLAGE_STREET_WIDTH_MAX}) | \
             plots {plots} | village plots/block {:?}",
            village_plots
        );
        println!("  collision prisms {}", index.prism_count());

        // ADR-0008 quality gate: plot coverage. An empty plot is a hole in
        // the wall-to-wall look the ADR asks for, so report both the mean
        // coverage per subdivided type and the share of empty plots.
        let mut buildings: Vec<(bevy::math::Vec2, f32)> = Vec::new();
        for cx in 0..GRID_CELLS as i32 {
            for cz in 0..GRID_CELLS as i32 {
                for o in plan_chunk(&graph, seed, (cx, cz), 1).objects {
                    if matches!(o.kind, PlacedKind::Building(_)) {
                        buildings.push((o.position, o.size.x * o.size.z));
                    }
                }
            }
        }
        for kind in [
            BlockType::UrbanVillage,
            BlockType::OldCity,
            BlockType::Apartment,
        ] {
            let mut covered = 0.0;
            let mut area = 0.0;
            let mut empty = 0;
            let mut total = 0;
            for block in graph.blocks.iter().filter(|b| b.kind == kind) {
                for child in &block.children {
                    total += 1;
                    let a = polygon_area(&child.polygon).abs();
                    area += a;
                    let c: f32 = buildings
                        .iter()
                        .filter(|(p, _)| point_in_convex_inclusive(*p, &child.polygon))
                        .map(|(_, s)| s)
                        .sum();
                    if c <= 0.1 {
                        empty += 1;
                    }
                    covered += c.min(a);
                }
            }
            if total > 0 {
                println!(
                    "  {kind:?}: plots {total}, coverage {:.0}%, empty plots {} ({:.0}%)",
                    covered / area.max(1.0) * 100.0,
                    empty,
                    empty as f32 / total as f32 * 100.0
                );
            }
        }
    }
}
