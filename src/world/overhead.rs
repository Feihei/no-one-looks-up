//! Cross-street overhead elements (R3.13): the layer that hangs **over** the
//! road instead of standing beside it.
//!
//! Everything before this module dressed the street from its edges inward: the
//! facades face the road, the poles stand on the curb, the trees fill the
//! setback. What a real old district has and that set cannot express is the
//! stuff strung across the gap -- drop cables from one side to the other, a
//! lamp bracket cantilevered out over the middle lane, a 防护棚 (protection
//! gantry) thrown up over the road while someone works on a roof. Those are the
//! lines a walker sees against the dusk sky, and they are also the only
//! geometry in the city that the camera passes *underneath*.
//!
//! # The one hard rule: nothing below [`OVERHEAD_MIN_CLEARANCE`]
//!
//! ADR-0006 splits the elements by where they touch the ground:
//!
//! | Part | Where it stands | Clearance rule |
//! |---|---|---|
//! | 电线杆 / 灯柱 / 棚架立柱 | curb + [`POLE_STANDOFF`], inside the block's collision prism | none needed: the walkable street is the negative space between blocks (ADR-0004) |
//! | 跨街电线 / 悬挑灯臂 / 棚架横梁+顶板 | over the road | underside >= 3.5 m, asserted at plan time |
//!
//! A vertical part placed on the curb line is already *outside the walkable
//! region*: the collision prisms are the block polygons, inset to the curb, so
//! the strip from curb outward is solid and a post there can never be walked
//! into. That is why the posts need no collision of their own -- and why they
//! must never drift into the street. `posts_stand_outside_the_walkable_street`
//! pins it against every edge of the graph, not just the one they were planned
//! on.
//!
//! The overhead parts are the opposite case: nothing can stop the camera, so
//! the rule has to be enforced by design. [`overhead_lowest`] is the single
//! source of truth for "how low does this instance actually hang" (a cable's
//! sag, a lamp housing's droop, a deck's underside), used by the planner's
//! debug assertion and by the test that walks every planned chunk.
//!
//! # Everything hangs off streams that already exist
//!
//! A cross-street cable has to meet a pole; a lamp bracket has to be bolted to
//! one; a scaffold must not grow around a trunk. So this module *replays* the
//! two existing streams -- [`props::pole_slots`] and
//! [`placement::street_tree_alongs`] -- instead of drawing from them, and
//! spends one dedicated RNG (salt 13000) on the decisions of its own. No
//! approved pole, tree or wear value moves by a single draw.
//!
//! # Geometry
//!
//! Scaffold parts are scaled instances of the shared unit cube and cylinder
//! (a box survives non-uniform scaling; a pipe is a cylinder), so a whole
//! gantry costs one draw call family. Only the two long-bracket meshes are
//! baked at real size, because a mount plate tapering into a drooping arm does
//! not survive being stretched.

use bevy::prelude::*;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::world::layout_graph::LayoutGraph;
use crate::world::placement::{self, BuildingParams, ChunkPlan};
use crate::world::props::{self, POLE_STANDOFF, PropKind};
use crate::world::roof::MeshBuf;

/// Lowest world height any overhead element may reach (m).
///
/// The camera walks at `EYE_HEIGHT = 1.65 m`. 3.5 m leaves 1.85 m of air above
/// the eye -- enough that a lamp bracket or a scaffold deck reads as *overhead*
/// rather than as a beam across the face, and enough that the near plane stays
/// clear at the steepest look-up pitch. Every constant below is designed above
/// this floor; `overhead_elements_clear_the_camera` asserts the instances.
pub const OVERHEAD_MIN_CLEARANCE: f32 = 3.5;

/// Mount heights of the two stacked cross-street cables (m).
///
/// Below the pole's own three-wire run (5.6-6.6) and below the crossarm, so the
/// street reads as two independent systems: the long run along the street, and
/// the short drop lines crossing it.
pub const CROSS_WIRE_HEIGHTS: [f32; 2] = [6.0, 6.5];

/// Share of pole pairs that carry a cross-street drop line.
pub const CROSS_WIRE_CHANCE: f32 = 0.4;

/// Sag baked into the unit cross-street span (m) at [`CROSS_WIRE_NOMINAL`].
///
/// A cross-street drop is strung tight over a short gap, so it sags far less
/// than the 30 m run along the street: 0.45 m over 10 m instead of 0.9 m.
pub const CROSS_WIRE_SAG: f32 = 0.45;

/// Span the cross-street mesh is baked for (m); also the scale reference for
/// the per-instance sag.
pub const CROSS_WIRE_NOMINAL: f32 = 10.0;

/// Cross-street cable radius (m). Slightly heavier than the run along the
/// street -- drop lines are insulated and read as a thicker line, and at 8-12 m
/// distance a 2 cm cable would disappear at dusk.
pub const CROSS_WIRE_RADIUS: f32 = 0.025;

/// Mount height of the long lamp bracket (m).
///
/// Above the crossarm (6.6) so the bracket never tangles with the wire run, and
/// clear of the pole top (8.0).
pub const LONG_LAMP_MOUNT: f32 = 7.1;

/// How far the long bracket reaches from the pole axis (m). From a curb +
/// 0.45 m pole this puts the head 3.75 m into the road -- over the middle of a
/// 6-10 m street.
pub const LONG_LAMP_REACH: f32 = 4.2;

/// Share of poles *without* a short bracket that get a long one.
pub const LONG_LAMP_CHANCE: f32 = 0.3;

/// How far the long bracket's head housing hangs below its mount (m).
pub const LONG_LAMP_DROP: f32 = 0.32;

/// How far the glowing lens hangs below the mount (m): 1.5 cm proud of the
/// housing, so it is actually visible from the street.
pub const LONG_LAMP_LENS_DROP: f32 = 0.335;

/// Share of streets that get a 防护棚.
///
/// Deliberately low: a gantry is a construction site, not street furniture, and
/// one per block would read as a parade. At 0.18 an 800 m map carries about a
/// dozen -- rare enough that meeting one is an event, common enough that a
/// half-hour walk finds one.
pub const SCAFFOLD_CHANCE: f32 = 0.18;

/// Underside of the scaffold's lowest beam (m) -- the clearance a walker gets.
pub const SCAFFOLD_CLEAR: f32 = 3.9;

/// Scaffold beam depth (m, vertical).
pub const SCAFFOLD_BEAM: f32 = 0.2;

/// Scaffold beam width (m, horizontal).
pub const SCAFFOLD_BEAM_WIDTH: f32 = 0.14;

/// Deck plank thickness (m).
pub const SCAFFOLD_DECK: f32 = 0.12;

/// Upright post diameter (m).
pub const SCAFFOLD_POST: f32 = 0.16;

/// Shortest / longest gantry along the street (m).
pub const SCAFFOLD_MIN_LEN: f32 = 6.0;
pub const SCAFFOLD_MAX_LEN: f32 = 14.0;

/// Step between candidate gantry centres along the edge (m).
pub const SCAFFOLD_STEP: f32 = 1.0;

/// Distance a gantry keeps from the edge ends (m), so no post lands in the
/// junction.
///
/// A street's corridor is `width / 2` either side of its centreline, and the
/// widest street the generator makes is `STREET_WIDTH_MAX` = 10 m. A post
/// 6 m past the edge node is therefore clear of the crossing street's
/// corridor -- 3 m was not: measured, a post stood 0.63 m inside a junction.
pub const SCAFFOLD_END_MARGIN: f32 = 6.0;

/// How far the longitudinal beams run past the posts (m).
pub const SCAFFOLD_BEAM_OVERHANG: f32 = 0.3;

/// Clearance a gantry keeps from a utility pole (m).
pub const SCAFFOLD_POLE_PAD: f32 = 0.6;

/// Clearance a gantry keeps from a street tree (m).
///
/// Not a trunk clearance: a 6-8 m tree's crown starts around 2 m and spreads
/// 1.5-2.5 m, so a gantry deck at 4.3 m would cut straight through it. The dead
/// zone has to cover the crown, not the trunk.
pub const SCAFFOLD_TREE_PAD: f32 = 2.2;

/// Lowest world height of one overhead instance (m), or `None` for parts that
/// stand on the ground (the posts).
///
/// The single source of truth for the clearance rule: the planner asserts it in
/// debug builds, and the test suite reads it back off real plans. Keeping the
/// per-kind sag and droop here -- instead of in the test -- is the point: a new
/// kind that forgets its own geometry shows up as a failing assertion rather
/// than as a camera clipping through a beam.
pub fn overhead_lowest(kind: PropKind, base: f32, size: Vec3) -> Option<f32> {
    match kind {
        // A post is planted on the ground; the player can never reach it
        // (it stands inside the block's collision prism).
        PropKind::ScaffoldPost => None,
        // The span's sag scales with `size.y` (see `bake_cross_wire`), so the
        // lowest point moves with the actual street width.
        PropKind::CrossWire => Some(base - CROSS_WIRE_SAG * size.y - CROSS_WIRE_RADIUS),
        PropKind::LongLampArm => Some(base - LONG_LAMP_DROP),
        PropKind::LongLampHead => Some(base - LONG_LAMP_LENS_DROP),
        PropKind::ScaffoldBeam | PropKind::ScaffoldDeck => Some(base),
        _ => None,
    }
}

/// Appends one overhead prop, asserting the clearance rule in debug builds.
#[allow(clippy::too_many_arguments)]
fn push_overhead(
    plan: &mut ChunkPlan,
    kind: PropKind,
    position: Vec2,
    yaw: f32,
    size: Vec3,
    base: f32,
    variant: u8,
) {
    if let Some(lowest) = overhead_lowest(kind, base, size) {
        debug_assert!(
            lowest >= OVERHEAD_MIN_CLEARANCE - 1e-3,
            "{kind:?} hangs at {lowest} m, under the {OVERHEAD_MIN_CLEARANCE} m clearance"
        );
    }
    props::push_prop(plan, kind, position, yaw, size, base, variant);
}

/// Plans the cross-street overhead layer of one edge.
///
/// Called once per edge (not per side): a span belongs to both curbs at once.
/// The caller has already established that the edge's midpoint anchors this
/// chunk, so nothing is duplicated across chunk seams.
///
/// Draw order is fixed and documented because the stream is per edge: all
/// cross-wire rolls, then all long-bracket rolls, then the gantry decision.
/// Anything added later belongs at the end.
pub fn place_overhead_furniture(
    plan: &mut ChunkPlan,
    graph: &LayoutGraph,
    seed: u64,
    edge_index: usize,
    tree_assets: u8,
) {
    let edge = &graph.edges[edge_index];
    let Some(from) = graph.nodes.get(edge.from as usize) else {
        return;
    };
    let Some(to) = graph.nodes.get(edge.to as usize) else {
        return;
    };
    let mid = (from.pos + to.pos) * 0.5;
    let dir = (to.pos - from.pos).normalize_or_zero();
    if dir == Vec2::ZERO {
        return;
    }
    let normal = Vec2::new(-dir.y, dir.x);
    let length = (to.pos - from.pos).length();
    let curb = edge.width * 0.5;
    let stand = curb + POLE_STANDOFF;

    // Both curbs must be building frontage: a plaza or forest side has no pole
    // line to hang from, and its block polygon is not solid, so a post there
    // would stand in walkable space.
    let mut params: [Option<BuildingParams>; 2] = [None; 2];
    for (i, block_id) in [edge.left_block, edge.right_block].into_iter().enumerate() {
        let Some(block) = block_id.and_then(|id| graph.blocks.get(id as usize)) else {
            continue;
        };
        params[i] = BuildingParams::of(block.kind);
    }
    if params[0].is_none() || params[1].is_none() {
        return;
    }

    let side_normals = [normal, -normal];
    let poles: [Vec<props::PoleSlot>; 2] = [
        props::pole_slots(seed, edge_index as u32, 0, length),
        props::pole_slots(seed, edge_index as u32, 1, length),
    ];
    // Where the street trees actually stand, replayed from the placement
    // stream. Empty when no tree asset is loaded, which is also when no tree
    // is planned -- the two stay consistent by construction.
    let trees: [Vec<f32>; 2] = [
        placement::street_tree_alongs(
            seed,
            edge_index as u32,
            0,
            params[0].map_or(0.0, |p| p.street_tree_probability),
            length,
            tree_assets,
        ),
        placement::street_tree_alongs(
            seed,
            edge_index as u32,
            1,
            params[1].map_or(0.0, |p| p.street_tree_probability),
            length,
            tree_assets,
        ),
    ];

    let mut rng = overhead_rng(seed, edge_index as u32);

    place_cross_wires(plan, &mut rng, mid, dir, normal, stand, &poles);
    place_long_brackets(plan, &mut rng, mid, dir, side_normals, stand, &poles);
    place_scaffold(
        plan, &mut rng, mid, dir, normal, stand, length, &poles, &trees,
    );
}

/// The cross-street drop lines: from pole `i` on one curb to pole `i` on the
/// other.
///
/// Both sides get the same number of slots (same edge length), so pairing by
/// index is well defined. The two poles are jittered independently, which
/// leaves the span slightly diagonal -- exactly how a real drop line looks.
fn place_cross_wires(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    mid: Vec2,
    dir: Vec2,
    normal: Vec2,
    stand: f32,
    poles: &[Vec<props::PoleSlot>; 2],
) {
    for (a, b) in poles[0].iter().zip(poles[1].iter()) {
        let roll: f32 = rng.random();
        if roll >= CROSS_WIRE_CHANCE {
            continue;
        }
        let start = mid + dir * a.along + normal * stand;
        let end = mid + dir * b.along - normal * stand;
        let span_vec = end - start;
        let span = span_vec.length();
        if span < 1.0 {
            continue;
        }
        let yaw = props::edge_yaw(span_vec);
        for (rung, height) in CROSS_WIRE_HEIGHTS.iter().enumerate() {
            push_overhead(
                plan,
                PropKind::CrossWire,
                start,
                yaw,
                Vec3::new(span, span / CROSS_WIRE_NOMINAL, 1.0),
                *height,
                rung as u8,
            );
        }
    }
}

/// The long lamp brackets: one per pole that did not already get a short one.
///
/// Both sides draw a roll for every pole, whether or not a bracket is placed,
/// so the stream does not shift when a side's pole line shortens.
fn place_long_brackets(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    mid: Vec2,
    dir: Vec2,
    side_normals: [Vec2; 2],
    stand: f32,
    poles: &[Vec<props::PoleSlot>; 2],
) {
    for (side, slots) in poles.iter().enumerate() {
        let normal = side_normals[side];
        let street_dir = -normal;
        let arm_yaw = props::edge_yaw(street_dir);
        let stand_line = mid + normal * stand;
        for slot in slots {
            let roll: f32 = rng.random();
            if slot.lamp || roll >= LONG_LAMP_CHANCE {
                continue;
            }
            let position = stand_line + dir * slot.along;
            push_overhead(
                plan,
                PropKind::LongLampArm,
                position,
                arm_yaw,
                Vec3::ONE,
                LONG_LAMP_MOUNT,
                0,
            );
            push_overhead(
                plan,
                PropKind::LongLampHead,
                position,
                arm_yaw,
                Vec3::ONE,
                LONG_LAMP_MOUNT,
                0,
            );
        }
    }
}

/// The 防护棚: posts on both curb lines, a beam grid over the road, and a deck
/// on top.
///
/// A gantry may not swallow a pole (the pole would spear the deck) or a tree
/// (a 4.3 m deck cuts through a 6-8 m crown), so the centre is scanned from a
/// stream-chosen starting slot until a clear bay is found. When the edge is too
/// crowded for its own length, no gantry appears -- the roll is still spent, so
/// the rest of the edge's stream is unaffected.
#[allow(clippy::too_many_arguments)]
fn place_scaffold(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    mid: Vec2,
    dir: Vec2,
    normal: Vec2,
    stand: f32,
    length: f32,
    poles: &[Vec<props::PoleSlot>; 2],
    trees: &[Vec<f32>; 2],
) {
    let roll: f32 = rng.random();
    if roll >= SCAFFOLD_CHANCE {
        return;
    }
    let longest = SCAFFOLD_MAX_LEN.min(length - 2.0 * SCAFFOLD_END_MARGIN);
    if longest < SCAFFOLD_MIN_LEN {
        return;
    }
    let span = rng.random_range(SCAFFOLD_MIN_LEN..=longest);
    let half = span * 0.5;

    let lo = -(length * 0.5 - half - SCAFFOLD_END_MARGIN);
    let hi = length * 0.5 - half - SCAFFOLD_END_MARGIN;
    if hi < lo {
        return;
    }
    let steps = (((hi - lo) / SCAFFOLD_STEP).floor() as usize + 1).max(1);
    let first = ((rng.random::<f32>() * steps as f32) as usize).min(steps - 1);

    let pole_alongs: Vec<f32> = poles.iter().flatten().map(|slot| slot.along).collect();
    let tree_alongs: Vec<f32> = trees.iter().flatten().copied().collect();

    let mut centre = None;
    for k in 0..steps {
        let index = (first + k) % steps;
        let candidate = lo + index as f32 * SCAFFOLD_STEP;
        let pole_clear = pole_alongs
            .iter()
            .all(|p| (p - candidate).abs() >= half + SCAFFOLD_POLE_PAD);
        let tree_clear = tree_alongs
            .iter()
            .all(|t| (t - candidate).abs() >= half + SCAFFOLD_TREE_PAD);
        if pole_clear && tree_clear {
            centre = Some(candidate);
            break;
        }
    }
    let Some(centre) = centre else {
        return;
    };

    let yaw = props::edge_yaw(dir);
    let at = |lx: f32, lz: f32| mid + dir * (centre + lx) + normal * lz;
    // Three stacked levels, each resting on the one below: beams, cross beams,
    // planks. Coincident faces are what makes a scaffold z-fight, so the levels
    // are separated by the thickness of what sits on them.
    let beam_line = SCAFFOLD_CLEAR;
    let cross_line = SCAFFOLD_CLEAR + SCAFFOLD_BEAM;
    let deck_line = SCAFFOLD_CLEAR + 2.0 * SCAFFOLD_BEAM;
    let half_width = stand;

    for lx in [-half, half] {
        for lz in [-half_width, half_width] {
            push_overhead(
                plan,
                PropKind::ScaffoldPost,
                at(lx, lz),
                yaw,
                Vec3::new(SCAFFOLD_POST, SCAFFOLD_CLEAR, SCAFFOLD_POST),
                0.0,
                0,
            );
        }
    }
    // Longitudinal beams over each post line.
    for lz in [-half_width, half_width] {
        push_overhead(
            plan,
            PropKind::ScaffoldBeam,
            at(0.0, lz),
            yaw,
            Vec3::new(
                span + 2.0 * SCAFFOLD_BEAM_OVERHANG,
                SCAFFOLD_BEAM,
                SCAFFOLD_BEAM_WIDTH,
            ),
            beam_line,
            0,
        );
    }
    // Cross beams: one at each end and one at mid-span, so the underside has a
    // rhythm instead of reading as one blank ceiling.
    for lx in [-half, 0.0, half] {
        push_overhead(
            plan,
            PropKind::ScaffoldBeam,
            at(lx, 0.0),
            yaw,
            Vec3::new(SCAFFOLD_BEAM_WIDTH, SCAFFOLD_BEAM, 2.0 * stand + 0.3),
            cross_line,
            1,
        );
    }
    push_overhead(
        plan,
        PropKind::ScaffoldDeck,
        at(0.0, 0.0),
        yaw,
        Vec3::new(span + 0.5, SCAFFOLD_DECK, 2.0 * stand + 0.3),
        deck_line,
        0,
    );
}

/// Overhead-layer RNG: a dedicated per-edge salt stream (13000), kept away from
/// the building/tree/furniture salts so no existing sequence shifts.
fn overhead_rng(seed: u64, edge: u32) -> StdRng {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23)
        ^ (edge as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ 13_000u64.wrapping_mul(0x1656_67B1_9E37_79F9);
    StdRng::seed_from_u64(mixed)
}

// --- mesh baking -----------------------------------------------------------

/// One cross-street cable: the shared catenary at its own sag and radius.
pub fn bake_cross_wire() -> Mesh {
    props::bake_catenary(CROSS_WIRE_SAG, CROSS_WIRE_RADIUS)
}

/// The long bracket: mount plate against the pole, a straight inner arm, a
/// drooping outer arm, and a head housing at the far end.
///
/// Baked at real size (the taper and the droop are the shape) with the mount at
/// the origin and the reach along local +x, like the short bracket.
pub fn bake_long_lamp_arm() -> Mesh {
    let mut buf = MeshBuf::new();
    // Mount plate bolted to the pole face.
    props::push_box(&mut buf, [-0.10, -0.11, -0.07], [0.10, 0.11, 0.07]);
    // Inner arm: horizontal off the pole.
    props::push_box(&mut buf, [0.0, -0.05, -0.05], [2.6, 0.03, 0.05]);
    // Outer arm: sags half a hand's width over the last stretch.
    props::push_box(
        &mut buf,
        [2.6, -0.11, -0.045],
        [LONG_LAMP_REACH - 0.34, -0.03, 0.045],
    );
    // Head housing, hanging under the outer arm's end.
    props::push_box(
        &mut buf,
        [LONG_LAMP_REACH - 0.34, -LONG_LAMP_DROP, -0.13],
        [LONG_LAMP_REACH, -0.06, 0.13],
    );
    buf.build()
}

/// The glowing lens under the long bracket's head housing.
///
/// Proud of the housing by 1.5 cm, for the same reason as [`props::bake_lamp_head`]:
/// a glowing box enclosed by a closed housing is invisible.
pub fn bake_long_lamp_head() -> Mesh {
    let mut buf = MeshBuf::new();
    let low = -LONG_LAMP_LENS_DROP;
    props::push_box(
        &mut buf,
        [LONG_LAMP_REACH - 0.30, low, -0.10],
        [LONG_LAMP_REACH - 0.04, low + 0.02, 0.10],
    );
    buf.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::world::geometry::dist_point_to_segment;
    use crate::world::path_network::generate_world;
    use crate::world::placement::{PlacedKind, PlacedObject, plan_chunk};
    use crate::world::{CELL_SIZE, CHUNK_SIZE, GRID_CELLS};

    const SEEDS: [u64; 2] = [7, crate::world::chunks::DEFAULT_SEED];

    fn overhead_kind(object: &PlacedObject) -> Option<PropKind> {
        let PlacedKind::Prop(kind) = object.kind else {
            return None;
        };
        overhead_lowest(kind, object.base, object.size).map(|_| kind)
    }

    #[test]
    fn overhead_elements_clear_the_camera() {
        for seed in SEEDS {
            let mut checked = 0;
            let (_, graph) = generate_world(seed).expect("generation");
            let cells = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32;
            for cx in 0..cells {
                for cz in 0..cells {
                    for object in plan_chunk(&graph, seed, (cx, cz), 3).objects {
                        let Some(kind) = overhead_kind(&object) else {
                            continue;
                        };
                        let lowest = overhead_lowest(kind, object.base, object.size)
                            .expect("kind has an overhead height");
                        assert!(
                            lowest >= OVERHEAD_MIN_CLEARANCE - 1e-3,
                            "seed {seed}: {kind:?} at {:?} hangs at {lowest:.2} m, \
                             under the {OVERHEAD_MIN_CLEARANCE} m clearance",
                            object.position
                        );
                        checked += 1;
                    }
                }
            }
            assert!(checked > 100, "seed {seed}: only {checked} overhead parts");
        }
    }

    #[test]
    fn posts_stand_outside_the_walkable_street() {
        // The posts are the only overhead parts that touch the ground, so they
        // are the only ones the walker could collide with. They must sit on the
        // block side of every curb -- not merely of their own edge, because a
        // junction corner is close enough for a post to lean into the cross
        // street.
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).expect("generation");
            let mut posts = 0;
            let cells = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32;
            for cx in 0..cells {
                for cz in 0..cells {
                    for object in plan_chunk(&graph, seed, (cx, cz), 3).objects {
                        if object.kind != PlacedKind::Prop(PropKind::ScaffoldPost) {
                            continue;
                        }
                        posts += 1;
                        let miss = graph
                            .edges
                            .iter()
                            .map(|edge| {
                                let a = graph.nodes[edge.from as usize].pos;
                                let b = graph.nodes[edge.to as usize].pos;
                                dist_point_to_segment(object.position, a, b) - edge.width * 0.5
                            })
                            .fold(f32::MAX, f32::min);
                        assert!(
                            miss >= -1e-3,
                            "seed {seed}: scaffold post at {:?} stands {:.2} m inside a street",
                            object.position,
                            -miss
                        );
                    }
                }
            }
            assert!(posts > 0, "seed {seed}: no scaffold posts planned");
        }
    }

    /// A synthetic edge along +x at (500, 500) with a 4 m curb, both sides
    /// built. `edge` keys the stream.
    const MID: Vec2 = Vec2::new(500.0, 500.0);
    const DIR: Vec2 = Vec2::new(1.0, 0.0);
    const NORMAL: Vec2 = Vec2::new(0.0, 1.0);
    const STAND: f32 = 4.0 + POLE_STANDOFF;

    #[test]
    fn cross_wires_join_poles_on_both_curbs() {
        // Scan edges until one carries a cable: a 0.4 roll per pole pair
        // makes "this particular edge has none" a coin flip, and the
        // assertion is about geometry, not about luck.
        let mut found = None;
        for edge in 0..24u32 {
            let mut plan = ChunkPlan::default();
            let mut rng = overhead_rng(9, edge);
            let poles = [
                props::pole_slots(9, edge, 0, 120.0),
                props::pole_slots(9, edge, 1, 120.0),
            ];
            place_cross_wires(&mut plan, &mut rng, MID, DIR, NORMAL, STAND, &poles);
            if !plan.objects.is_empty() {
                found = Some(plan.objects);
                break;
            }
        }
        let wires = found.expect("no edge in 24 produced a cross-street cable");
        assert_eq!(
            wires.len() % CROSS_WIRE_HEIGHTS.len(),
            0,
            "wires come in whole runs of {}",
            CROSS_WIRE_HEIGHTS.len()
        );
        for wire in &wires {
            // One end sits on a pole on this curb, the other on a pole of the
            // opposite curb; the span is the distance between them.
            let reach = Vec2::new(wire.yaw.cos(), -wire.yaw.sin());
            let end = wire.position + reach * wire.size.x;
            let own_curb = (wire.position - MID).dot(NORMAL);
            let far_curb = (end - MID).dot(NORMAL);
            assert!(
                (own_curb - STAND).abs() < 1e-3,
                "the near end must be on the pole line, got {own_curb}"
            );
            assert!(
                (far_curb + STAND).abs() < 1e-3,
                "the far end must be on the opposite pole line, got {far_curb}"
            );
            assert!(
                CROSS_WIRE_HEIGHTS.contains(&wire.base),
                "unexpected mount height {}",
                wire.base
            );
        }
    }

    #[test]
    fn long_brackets_only_replace_poles_without_a_short_one() {
        let mut found = None;
        for edge in 0..24u32 {
            let mut plan = ChunkPlan::default();
            let mut rng = overhead_rng(11, edge);
            let poles = [
                props::pole_slots(11, edge, 0, 150.0),
                props::pole_slots(11, edge, 1, 150.0),
            ];
            place_long_brackets(
                &mut plan,
                &mut rng,
                MID,
                DIR,
                [NORMAL, -NORMAL],
                STAND,
                &poles,
            );
            if !plan.objects.is_empty() {
                found = Some((plan.objects, poles));
                break;
            }
        }
        let (objects, poles) = found.expect("no edge in 24 produced a long bracket");
        let arms: Vec<&PlacedObject> = objects
            .iter()
            .filter(|o| o.kind == PlacedKind::Prop(PropKind::LongLampArm))
            .collect();
        assert_eq!(
            arms.len() * 2,
            objects.len(),
            "every long bracket carries its lens and nothing else is placed"
        );
        for arm in &arms {
            let reach = Vec2::new(arm.yaw.cos(), -arm.yaw.sin());
            let own = (arm.position - MID).dot(NORMAL);
            let (side, street) = if own > 0.0 {
                (0usize, -NORMAL)
            } else {
                (1usize, NORMAL)
            };
            assert!(
                reach.dot(street) > 0.999,
                "the bracket must reach across the road"
            );
            let along = (arm.position - MID).dot(DIR);
            assert!(
                poles[side]
                    .iter()
                    .all(|slot| !slot.lamp || (slot.along - along).abs() > 1e-3),
                "a pole with a short bracket must not get a second one"
            );
            assert!(
                objects
                    .iter()
                    .any(|o| o.kind == PlacedKind::Prop(PropKind::LongLampHead)
                        && o.position == arm.position
                        && o.base == arm.base),
                "every long bracket carries its lens"
            );
        }
    }

    #[test]
    fn scaffolds_span_the_street_between_their_posts() {
        // No poles or trees anywhere near, so the bay scan always succeeds;
        // only the 0.1 chance roll can say no, so scan edges for one that says
        // yes.
        let empty: [Vec<props::PoleSlot>; 2] = [Vec::new(), Vec::new()];
        let no_trees: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
        let mut found = None;
        for edge in 0..64u32 {
            let mut plan = ChunkPlan::default();
            let mut rng = overhead_rng(5, edge);
            place_scaffold(
                &mut plan, &mut rng, MID, DIR, NORMAL, STAND, 50.0, &empty, &no_trees,
            );
            if !plan.objects.is_empty() {
                found = Some(plan.objects);
                break;
            }
        }
        let objects = found.expect("no edge in 64 raised a gantry");

        let posts: Vec<&PlacedObject> = objects
            .iter()
            .filter(|o| o.kind == PlacedKind::Prop(PropKind::ScaffoldPost))
            .collect();
        assert_eq!(posts.len(), 4, "a gantry stands on four posts");
        for post in &posts {
            assert!(
                ((post.position - MID).dot(NORMAL).abs() - STAND).abs() < 1e-3,
                "posts stand on the curb line"
            );
        }
        let deck = objects
            .iter()
            .find(|o| o.kind == PlacedKind::Prop(PropKind::ScaffoldDeck))
            .expect("a gantry carries a deck");
        assert!(
            (deck.size.z - (2.0 * STAND + 0.3)).abs() < 1e-3,
            "the deck spans curb to curb"
        );
        assert!(
            deck.base >= SCAFFOLD_CLEAR + 2.0 * SCAFFOLD_BEAM - 1e-3,
            "the deck sits on the beams"
        );
        // Every beam is above the clear height, or the walker would hit it.
        for beam in objects
            .iter()
            .filter(|o| o.kind == PlacedKind::Prop(PropKind::ScaffoldBeam))
        {
            assert!(beam.base >= SCAFFOLD_CLEAR - 1e-4);
        }
        // The deck must cover the street, not just the middle of it.
        assert!(
            deck.size.z >= 2.0 * STAND,
            "a deck narrower than the street would leave the curb line open"
        );
    }

    #[test]
    fn a_gantry_never_grows_around_a_pole_or_a_tree() {
        // Feed the bay scan a pole and a tree right where it would start and
        // check it slides away from both.
        let poles = [
            vec![props::PoleSlot {
                along: 0.0,
                lamp: false,
                crossarm_jitter: 0.0,
            }],
            Vec::new(),
        ];
        let trees = [vec![0.0f32], Vec::new()];
        for edge in 0..64u32 {
            let mut plan = ChunkPlan::default();
            let mut rng = overhead_rng(3, edge);
            place_scaffold(
                &mut plan, &mut rng, MID, DIR, NORMAL, STAND, 50.0, &poles, &trees,
            );
            let posts: Vec<&PlacedObject> = plan
                .objects
                .iter()
                .filter(|o| o.kind == PlacedKind::Prop(PropKind::ScaffoldPost))
                .collect();
            if posts.is_empty() {
                // Also acceptable: no bay clear of both obstacles.
                continue;
            }
            for post in posts {
                let along = (post.position - MID).dot(DIR);
                assert!(
                    along.abs() >= SCAFFOLD_POLE_PAD,
                    "a post ate the pole at its own along offset"
                );
                assert!(
                    along.abs() >= SCAFFOLD_TREE_PAD - 1e-3,
                    "a post grew into the tree's crown space"
                );
            }
        }
    }

    #[test]
    fn overhead_stream_is_stable_and_edge_keyed() {
        let furniture = |edge: u32| {
            let mut plan = ChunkPlan::default();
            let mut rng = overhead_rng(7, edge);
            let mid = Vec2::new(100.0, 100.0);
            let dir = Vec2::new(1.0, 0.0);
            let normal = Vec2::new(0.0, 1.0);
            let poles = [
                props::pole_slots(7, edge, 0, 120.0),
                props::pole_slots(7, edge, 1, 120.0),
            ];
            place_cross_wires(&mut plan, &mut rng, mid, dir, normal, 4.45, &poles);
            plan.objects
        };
        assert_eq!(furniture(3), furniture(3), "same edge, same overhead");
        let differing = (0..30u32).filter(|e| furniture(*e) != furniture(3)).count();
        assert!(
            differing >= 25,
            "edge keying collapsed: {differing}/30 differ"
        );
    }

    #[test]
    fn the_layer_exists_at_all() {
        // A guard against a probability table that quietly never fires.
        let seed = SEEDS[1];
        let (_, graph) = generate_world(seed).expect("generation");
        let cells = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32;
        let mut wires = 0;
        let mut arms = 0;
        let mut posts = 0;
        for cx in 0..cells {
            for cz in 0..cells {
                for object in plan_chunk(&graph, seed, (cx, cz), 3).objects {
                    match object.kind {
                        PlacedKind::Prop(PropKind::CrossWire) => wires += 1,
                        PlacedKind::Prop(PropKind::LongLampArm) => arms += 1,
                        PlacedKind::Prop(PropKind::ScaffoldPost) => posts += 1,
                        _ => {}
                    }
                }
            }
        }
        assert!(wires > 50, "cross-street cables: {wires}");
        assert!(arms > 20, "long brackets: {arms}");
        assert!(posts > 4, "scaffold posts: {posts}");
    }

    #[test]
    fn baked_overhead_meshes_are_sound() {
        // The cross-street cable is the shared catenary at its own sag/radius.
        let mesh = bake_cross_wire();
        let positions: Vec<[f32; 3]> = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("positions")
            .as_float3()
            .expect("f32")
            .to_vec();
        let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
        let (mut min_x, mut max_x) = (f32::MAX, f32::MIN);
        for p in &positions {
            min_y = min_y.min(p[1]);
            max_y = max_y.max(p[1]);
            min_x = min_x.min(p[0]);
            max_x = max_x.max(p[0]);
        }
        assert!(
            min_x.abs() < 1e-6 && (max_x - 1.0).abs() < 1e-6,
            "unit span"
        );
        assert!(
            (min_y + CROSS_WIRE_SAG + CROSS_WIRE_RADIUS).abs() < 1e-4,
            "sag baked at the middle: {min_y}"
        );
        assert!(max_y <= CROSS_WIRE_RADIUS + 1e-5, "hangs, never rises");

        // The long bracket reaches exactly `LONG_LAMP_REACH` and hangs no
        // lower than its lens.
        for (mesh, name) in [
            (bake_long_lamp_arm(), "long lamp arm"),
            (bake_long_lamp_head(), "long lamp head"),
        ] {
            let positions: Vec<[f32; 3]> = mesh
                .attribute(Mesh::ATTRIBUTE_POSITION)
                .expect("positions")
                .as_float3()
                .expect("f32")
                .to_vec();
            assert!(!positions.is_empty(), "{name} baked empty");
            let min_y = positions.iter().map(|p| p[1]).fold(f32::MAX, f32::min);
            let max_x = positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
            assert!(
                max_x <= LONG_LAMP_REACH + 1e-4,
                "{name} overshoots the reach"
            );
            assert!(
                min_y >= -LONG_LAMP_LENS_DROP - 1e-4,
                "{name} hangs below its lens"
            );
        }
        // The lens must be proud of the housing, or it is occluded.
        let arm_min = bake_long_lamp_arm()
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("positions")
            .as_float3()
            .expect("f32")
            .to_vec()
            .iter()
            .map(|p| p[1])
            .fold(f32::MAX, f32::min);
        assert!(
            -LONG_LAMP_LENS_DROP < arm_min,
            "the lens must hang below the housing ({arm_min})"
        );
    }
}
