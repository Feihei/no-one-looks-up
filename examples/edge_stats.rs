//! Diagnostic: per-block-type edge association coverage.
//!
//! Run with `cargo run --example edge_stats`. Checks that every street
//! segment knows which block sits on each side (R3.1 placement depends on it)
//! and prints the distribution so a regression shows up as a number, not a
//! missing-building mystery.

use no_one_looks_up::world::layout_graph::BlockType;
use no_one_looks_up::world::path_network::generate_world;
use std::collections::HashMap;

fn main() {
    for seed in [1u64, 7, 0x4845_4144] {
        let (_, g) = generate_world(seed).unwrap();
        let mut both = 0;
        let mut one = 0;
        let mut none = 0;
        let mut by_kind: HashMap<&str, usize> = HashMap::new();
        for e in &g.edges {
            match (e.left_block, e.right_block) {
                (Some(l), Some(r)) => {
                    both += 1;
                    let _ = (l, r);
                }
                (Some(_), None) | (None, Some(_)) => {
                    one += 1;
                    let id = e.left_block.or(e.right_block).unwrap();
                    let k = match g.blocks[id as usize].kind {
                        BlockType::OldCity => "old",
                        BlockType::UrbanVillage => "village",
                        BlockType::Apartment => "apt",
                        BlockType::Factory => "fac",
                        BlockType::Plaza => "plaza",
                        BlockType::Forest => "forest",
                    };
                    *by_kind.entry(k).or_default() += 1;
                }
                (None, None) => none += 1,
            }
        }
        println!(
            "seed {seed:#x}: edges {:4}  both {both:4}  one {one:4} (ring)  none {none:3}  {by_kind:?}",
            g.edges.len()
        );
        assert_eq!(none, 0, "every street segment flanks at least one block");
    }
}
