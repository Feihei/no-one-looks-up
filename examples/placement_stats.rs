//! Diagnostic: placement yield per world (R3.1/R3.2).
//!
//! Run with `cargo run --example placement_stats`. Prints how many objects of
//! each kind a full 40x40 map plans, plus the rejection rate implied by the
//! per-edge attempts. Numbers, not vibes: a placement regression shows up here
//! before it shows up on screen.

use no_one_looks_up::world::layout_graph::BlockType;
use no_one_looks_up::world::path_network::generate_world;
use no_one_looks_up::world::placement::{PlacedKind, plan_chunk};
use no_one_looks_up::world::props::PropKind;
use std::collections::HashMap;

fn main() {
    for seed in [1u64, 7, 0x4845_4144] {
        let (_, graph) = generate_world(seed).unwrap();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        let mut heights: HashMap<&str, (f32, f32)> = HashMap::new();
        for cx in 0..40 {
            for cz in 0..40 {
                let plan = plan_chunk(&graph, seed, (cx, cz), 3);
                for o in plan.objects {
                    let key = match o.kind {
                        PlacedKind::Building(BlockType::OldCity) => "building/old",
                        PlacedKind::Building(BlockType::Apartment) => "building/apt",
                        PlacedKind::Building(BlockType::Factory) => "building/fac",
                        PlacedKind::Building(_) => "building/other",
                        PlacedKind::StreetTree => "tree/street",
                        PlacedKind::ForestTree => "tree/forest",
                        PlacedKind::PlazaTree => "tree/plaza",
                        PlacedKind::PlazaPad => "plaza/pad",
                        PlacedKind::Prop(kind) => match kind {
                            PropKind::Awning => "prop/awning",
                            PropKind::AcUnit => "prop/ac",
                            PropKind::Downspout => "prop/pipe",
                            PropKind::Balcony => "prop/balcony",
                            PropKind::WaterTank => "prop/tank",
                            PropKind::Chimney => "prop/chimney",
                            PropKind::StairBox => "prop/stair",
                            PropKind::RoofVent => "prop/vent",
                            PropKind::Pole => "prop/pole",
                            PropKind::LampArm => "prop/lamp_arm",
                            PropKind::LampHead => "prop/lamp_head",
                            PropKind::CrossArm => "prop/crossarm",
                            PropKind::Wire => "prop/wire",
                            PropKind::CrossWire => "prop/xwire",
                            PropKind::LongLampArm => "prop/long_arm",
                            PropKind::LongLampHead => "prop/long_head",
                            PropKind::ScaffoldPost => "prop/scaf_post",
                            PropKind::ScaffoldBeam => "prop/scaf_beam",
                            PropKind::ScaffoldDeck => "prop/scaf_deck",
                        },
                    };
                    *counts.entry(key).or_default() += 1;
                    if matches!(o.kind, PlacedKind::Building(_)) {
                        let e = heights.entry(key).or_insert((f32::MAX, 0.0));
                        e.0 = e.0.min(o.size.y);
                        e.1 = e.1.max(o.size.y);
                    }
                }
            }
        }
        let mut keys: Vec<&str> = counts.keys().copied().collect();
        keys.sort_unstable();
        println!("seed {seed:#x}:");
        for k in keys {
            let h = heights
                .get(k)
                .map(|(lo, hi)| format!("  h {lo:.1}-{hi:.1}m"))
                .unwrap_or_default();
            println!("  {k:14} {:6}{h}", counts[k]);
        }
    }
}
