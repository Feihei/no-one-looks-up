//! Facade and rooftop props (R3.11 step 11.4) and street furniture
//! (step 11.5) -- the clutter that turns a textured box into a building
//! someone lives in, and a street gap into a street.
//!
//! ADR-0001 fixes the camera at 22.55-60 degrees of elevation, so the wall
//! strip that reads is the upper one, and the roofline is the highest-value
//! strip in the city. The prop tables are weighted by exactly that: awnings
//! sit under the first-floor windows, AC condensers dot the wall bands, and
//! the roofs grow water tanks, stair bulkheads, stove pipes and vents --
//! silhouettes against the dusk sky, not furniture for a dollhouse view
//! nobody has. The streets get the same treatment from the other side:
//! utility poles on the curb line, lamp arms reaching over the road with
//! emissive heads (the lamps are on at golden hour), and catenary wires
//! sagging between the poles -- lines against the sky are what a walk
//! through an old district actually looks like.
//!
//! Everything here touches the ground or a wall. The layer that hangs **over**
//! the road -- cross-street cables, long cantilevered lamp brackets, scaffolds
//! -- lives in [`crate::world::overhead`], because it answers to a different
//! rule (ADR-0006): nothing up there may come within 3.5 m of the street.
//!
//! # Everything is a shared mesh, scaled per instance
//!
//! Like the roofs (11.3): one unit cube and one unit cylinder cover every
//! box- or cylinder-shaped prop; the awning, balcony, lamp arm, lamp head
//! and wire are baked once at real size because their shape (a tilted
//! slab, a slab with a railing, a sagging catenary) does not survive
//! non-uniform scaling cleanly. Materials are shared city-wide, with a
//! rust variant chosen by the building's wear -- the 锈蚀外挂件 the design
//! asks for -- so a streaming window costs a handful of draw calls no
//! matter how much clutter it carries.
//!
//! # Props are position-keyed, never stream-keyed
//!
//! The per-edge RNG streams are already spoken for (placement order, wear,
//! facade variants). Every extra draw would shift the approved weathering
//! of every later building on the edge, so building props draw from their
//! own generator seeded by the building's exact position bits -- the same
//! trick `facade_variant` uses, and for the same reason -- and the street
//! furniture gets a dedicated salt stream of its own.

use bevy::prelude::*;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::world::facade::FacadeStyle;
use crate::world::placement::{ChunkPlan, PlacedKind, PlacedObject};
use crate::world::roof::MeshBuf;

/// What a prop is. The spawn side maps each kind to one shared mesh and one
/// shared material.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PropKind {
    /// 雨棚: tilted canvas over an old-city shopfront bay.
    Awning,
    /// 空调外机: wall-mounted condenser box.
    AcUnit,
    /// 落水管: vertical drainpipe hugging the facade.
    Downspout,
    /// 阳台: slab + railing strip on an apartment facade (one baked mesh).
    Balcony,
    /// 水箱: roof water tank on a flat (parapet) roof.
    WaterTank,
    /// 烟囱: a kitchen stove pipe through old-city gables, a tall stack on
    /// factory sheds.
    Chimney,
    /// Rooftop stair bulkhead poking above an apartment parapet.
    StairBox,
    /// Small vent between factory sawtooth teeth.
    RoofVent,
    /// 电线杆: concrete utility pole on the curb line.
    Pole,
    /// Street-lamp bracket bolted below the pole top, reaching over the road.
    LampArm,
    /// The lamp head at the arm's end -- the one emissive prop in the city.
    LampHead,
    /// 电线杆横担: the crossarm at the pole top the wires meet.
    CrossArm,
    /// One catenary wire span between two consecutive poles.
    Wire,
    /// 跨街电线: a cable strung from a pole on one curb to the pole opposite
    /// it (R3.13, see `world::overhead`).
    CrossWire,
    /// 悬挑灯臂: a long bracket off a pole reaching over the middle of the
    /// road, with [`PropKind::LongLampHead`] glowing at its tip.
    LongLampArm,
    /// The lamp head at the end of a long bracket.
    LongLampHead,
    /// 棚架立柱: a scaffold upright on the curb line, outside the walkable
    /// street. Stands on the ground, so it is not part of the overhead
    /// clearance rule.
    ScaffoldPost,
    /// 棚架横梁: a scaffold beam over the road (longitudinal or transverse).
    ScaffoldBeam,
    /// 棚架顶板: the deck planks a scaffold carries over the street.
    ScaffoldDeck,
}

/// Height of the 雨棚 wall mount above the ground (m): just under the
/// first-floor window line of a 3 m storey.
pub const AWNING_MOUNT: f32 = 2.5;

/// AC condenser depth (m): how far the box sticks out of the wall.
pub const AC_DEPTH: f32 = 0.3;

/// Cap on AC condensers per apartment, so the per-bay roll cannot run away
/// on a 12-bay, 6-floor slab.
pub const APARTMENT_AC_CAP: usize = 14;

/// Utility pole height (m). Tall enough that the lowest wire clears the
/// tallest old-city ridge (6 m wall + ~1.27 m gable) with room to spare.
pub const POLE_HEIGHT: f32 = 8.0;

/// Pole offset from the curb, into the setback strip (m). Real poles hug
/// the curb; the street trees stand further in (setback / 2), so the two
/// lines rarely share a spot.
pub const POLE_STANDOFF: f32 = 0.45;

/// Nominal pole spacing along an edge (m). Edges are ~50 m cell sides, so
/// most get two poles and one wire span.
pub const POLE_SPACING: f32 = 30.0;

/// Wire attachment heights on the pole (m), stacked the way concrete poles
/// carry a three-wire run.
pub const WIRE_HEIGHTS: [f32; 3] = [5.6, 6.1, 6.6];

/// Height of the crossarm beam (m): exactly at the top wire, so the run meets
/// the arm rather than floating above it.
pub const CROSSARM_MOUNT: f32 = 6.6;

/// Half-length of the crossarm (m). 1.6 m end to end reads as a real pole top
/// at street distance while staying shorter than the pole spacing.
pub const CROSSARM_HALF: f32 = 0.8;

/// Largest yaw deviation of a crossarm from square-across-the-street (rad).
///
/// Real pole tops are never perfectly aligned: each crossarm is set slightly
/// askew. A jitter stays small enough that the three wire runs still read as
/// one line from the street, but neighbouring poles no longer look stamped
/// from the same mould. The design note says "绕 Z 轴" because the original
/// Godot project is Z-up; in Bevy's Y-up world the vertical axis is Y, so this
/// is a `Quat::from_rotation_y` yaw.
pub const CROSSARM_YAW_JITTER: f32 = 0.17;

/// Lamp bracket mount height below the pole top (m).
pub const LAMP_MOUNT: f32 = 7.5;

/// Lamp head emissive strength. The lamps are ON at golden hour: a warm
/// dot per pole, picked up by the bloom pass, no real light budget spent.
pub const LAMP_EMISSIVE: f32 = 4.0;

/// Distance from the camera at which a lamp's emissive lens is switched off
/// (m). Applies to both the short bracket's head and the long cross-street
/// one.
///
/// The lens is a 2 cm-tall emissive sliver hanging under its shade, and the
/// scene camera runs `Msaa::Off` (bevy_elements needs a single-sample depth
/// prepass for its cloud pass). A sliver narrower than a pixel row is not
/// faded by anything: it covers the row on one frame and misses it on the
/// next, and bloom turns that on/off into a twinkle. At the scene's 45°
/// vertical FOV and 1080p, the lens clears a full pixel row only inside ~50 m,
/// is 0.65 px tall at 80 m, and is a point past 100 m. 60 m is where the dot
/// has already stopped being a resolved shape: cutting there costs nothing the
/// eye was reading and removes the whole range that twinkles.
///
/// No fade band, on purpose. `VisibilityRange`'s crossfade is a screen-space
/// dither, and on a 2 px dot a dither only trades one flicker for another for
/// the length of the approach. The matte shade above the lens stays put, so
/// the fixture still reads down the street -- only the light goes out.
pub const LAMP_GLOW_RANGE: f32 = 60.0;

/// Wire sag baked into the unit-span mesh, at [`WIRE_NOMINAL`] span (m).
/// Instances scale y by `span / WIRE_NOMINAL`, so the dip grows with the
/// span the way a real catenary does.
pub const WIRE_SAG: f32 = 0.9;

/// Span the wire mesh is baked for (m); also the scale reference for the
/// per-instance sag.
pub const WIRE_NOMINAL: f32 = 30.0;

/// Wire radius (m). A 2 cm cable at dusk distance reads as a clean line
/// against the sky without blooming into a noodle.
pub const WIRE_RADIUS: f32 = 0.02;

/// Appends one prop. `variant` reuses the facade variant slot as a material
/// selector: 0 = standard finish, 1 = rusted (or the second awning canvas).
pub(crate) fn push_prop(
    plan: &mut ChunkPlan,
    kind: PropKind,
    position: bevy::math::Vec2,
    yaw: f32,
    size: Vec3,
    base: f32,
    variant: u8,
) {
    plan.objects.push(PlacedObject {
        kind: PlacedKind::Prop(kind),
        position,
        yaw,
        size,
        footprint_half: bevy::math::Vec2::ZERO,
        wear: 0.0,
        variant,
        tree: None,
        base,
    });
}

/// Per-building RNG for props, keyed on the exact position bits.
///
/// Positions are produced by the same arithmetic on every plan, so the bits
/// are stable across runs; no quantisation is needed (unlike
/// `facade_variant`, which must survive shader-side float drift).
fn prop_rng(position: bevy::math::Vec2) -> StdRng {
    let mixed = (position.x.to_bits() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (position.y.to_bits() as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ 0x1656_67B1_9E37_79F9;
    StdRng::seed_from_u64(mixed)
}

/// Centre offset along the facade of bay `b` of `bays` across length `l`.
fn bay_u(b: usize, bays: usize, l: f32) -> f32 {
    let step = l / bays as f32;
    -l * 0.5 + (b as f32 + 0.5) * step
}

/// A random spot on the roof deck that keeps a `size` prop inside the
/// footprint with a small margin.
///
/// Rooftop clutter used to sit on the building's centreline at a couple of
/// fixed offsets, so a terrace of same-style apartments repeated the exact
/// same boxes -- a regular sawtooth silhouette along the skyline instead of
/// individual rooftops. Spreading the props over the deck, with the size
/// chosen by the caller, breaks that rhythm.
fn roof_spot(
    rng: &mut StdRng,
    along: bevy::math::Vec2,
    street: bevy::math::Vec2,
    l: f32,
    d: f32,
    size: Vec3,
) -> bevy::math::Vec2 {
    // Stay inside the footprint, and inside the 0.45 * length band the roof
    // props are allowed to occupy (the same limit the placement test asserts).
    let u_max = (((l - size.x).max(0.0) * 0.5 - 0.25).max(0.0)).min(l * 0.45);
    let v_max = (((d - size.z).max(0.0) * 0.5 - 0.25).max(0.0)).min(d * 0.45);
    along * rng.random_range(-u_max..=u_max) + street * rng.random_range(-v_max..=v_max)
}

/// Hangs the facade and rooftop props of one building (11.4).
///
/// `street` is the unit vector from the building centre toward the street
/// (the negated per-edge normal the building was placed with). Facade props
/// are pushed onto the plane `street * depth/2` from the centre; roof props
/// sit over the footprint at the wall-top height.
pub fn place_building_props(
    plan: &mut ChunkPlan,
    building: &PlacedObject,
    style: FacadeStyle,
    street: bevy::math::Vec2,
) {
    let mut rng = prop_rng(building.position);
    let (l, h, d) = (building.size.x, building.size.y, building.size.z);
    let along = bevy::math::Vec2::new(street.y, -street.x);
    let face = building.position + street * (d * 0.5);
    // Props that care about facing (awning tilt, balcony railing) are baked
    // with their outward side toward local +Z. Flip the yaw when the street
    // sits on the building's local -Z instead.
    let front = street.x * building.yaw.sin() + street.y * building.yaw.cos();
    let out_yaw = building.yaw
        + if front < 0.0 {
            std::f32::consts::PI
        } else {
            0.0
        };
    // 锈蚀外挂件: heavily worn buildings get rusty fixtures far more often
    // than fresh ones. Variant 1 selects the rust material at spawn time.
    let rust_chance = if building.wear > 0.6 { 0.65 } else { 0.2 };

    match style {
        FacadeStyle::OldCity => {
            // 雨棚: one canvas per shopfront bay, roughly half the bays.
            let bays = (l / 2.0).round().max(1.0) as usize;
            for b in 0..bays {
                if rng.random::<f32>() >= 0.45 {
                    continue;
                }
                let canvas = u8::from(rng.random::<f32>() < 0.5);
                push_prop(
                    plan,
                    PropKind::Awning,
                    face + along * bay_u(b, bays, l),
                    out_yaw,
                    Vec3::ONE,
                    AWNING_MOUNT,
                    canvas,
                );
            }
            // 空调外机: two-storey houses get a pair under the upper window
            // line; single-storey ones get one above the door.
            if h >= 6.0 {
                for u in [-l * 0.25, l * 0.25] {
                    if rng.random::<f32>() < 0.5 {
                        let variant = u8::from(rng.random::<f32>() < rust_chance);
                        push_prop(
                            plan,
                            PropKind::AcUnit,
                            face + along * u + street * (AC_DEPTH * 0.5),
                            building.yaw,
                            Vec3::new(0.75, 0.55, AC_DEPTH),
                            3.3,
                            variant,
                        );
                    }
                }
            } else if rng.random::<f32>() < 0.25 {
                let variant = u8::from(rng.random::<f32>() < rust_chance);
                push_prop(
                    plan,
                    PropKind::AcUnit,
                    face + street * (AC_DEPTH * 0.5),
                    building.yaw,
                    Vec3::new(0.75, 0.55, AC_DEPTH),
                    1.9,
                    variant,
                );
            }
            // 落水管: one per building, at a facade end.
            if rng.random::<f32>() < 0.35 {
                let side = if rng.random::<bool>() { 1.0 } else { -1.0 };
                let variant = u8::from(rng.random::<f32>() < rust_chance);
                push_prop(
                    plan,
                    PropKind::Downspout,
                    face + along * (side * (l * 0.5 - 0.15)),
                    building.yaw,
                    Vec3::new(0.18, h, 0.18),
                    0.0,
                    variant,
                );
            }
            // 烟囱: kitchen stove pipe through the gable.
            if rng.random::<f32>() < 0.3 {
                let u = rng.random_range(-l * 0.3..=l * 0.3);
                push_prop(
                    plan,
                    PropKind::Chimney,
                    building.position + along * u,
                    building.yaw,
                    Vec3::new(0.36, 1.8, 0.36),
                    h - 0.3,
                    0,
                );
            }
        }
        FacadeStyle::Apartment => {
            let bays = (l / 3.3).round().max(1.0) as usize;
            let floors = (h / 3.0).floor() as i32;
            // 阳台: every other bay, most floors above the ground one.
            let mut b = 0;
            while b < bays {
                let u = bay_u(b, bays, l);
                for floor in 1..floors {
                    if rng.random::<f32>() < 0.6 {
                        push_prop(
                            plan,
                            PropKind::Balcony,
                            face + along * u,
                            out_yaw,
                            Vec3::ONE,
                            floor as f32 * 3.0,
                            0,
                        );
                    }
                }
                b += 2;
            }
            // 空调外机 grid: one roll per (bay, floor), capped.
            let mut hung = 0;
            for floor in 0..floors {
                for b in 0..bays {
                    if hung >= APARTMENT_AC_CAP {
                        break;
                    }
                    if rng.random::<f32>() < 0.18 {
                        let variant = u8::from(rng.random::<f32>() < rust_chance);
                        push_prop(
                            plan,
                            PropKind::AcUnit,
                            face + along * bay_u(b, bays, l) + street * (AC_DEPTH * 0.5),
                            building.yaw,
                            Vec3::new(0.75, 0.55, AC_DEPTH),
                            floor as f32 * 3.0 + 0.3,
                            variant,
                        );
                        hung += 1;
                    }
                }
            }
            // 落水管: full-height pipe at a facade end.
            if rng.random::<f32>() < 0.5 {
                let side = if rng.random::<bool>() { 1.0 } else { -1.0 };
                let variant = u8::from(rng.random::<f32>() < rust_chance);
                push_prop(
                    plan,
                    PropKind::Downspout,
                    face + along * (side * (l * 0.5 - 0.15)),
                    building.yaw,
                    Vec3::new(0.18, h, 0.18),
                    0.0,
                    variant,
                );
            }
            // 水箱: flat parapet roof, so the tanks stand on the deck.
            //
            // Density is down from the first pass (always a tank, half of the
            // time a second one) and the size/spot now vary per building. The
            // fixed 1.9 m cube on the centreline made a terrace row read as one
            // repeated sawtooth of boxes rather than as separate rooftops.
            if rng.random::<f32>() < 0.5 {
                let size = Vec3::new(
                    rng.random_range(1.3..=2.2),
                    rng.random_range(1.4..=2.6),
                    rng.random_range(1.3..=2.2),
                );
                push_prop(
                    plan,
                    PropKind::WaterTank,
                    building.position + roof_spot(&mut rng, along, street, l, d, size),
                    building.yaw,
                    size,
                    h,
                    0,
                );
                if rng.random::<f32>() < 0.3 {
                    let size = Vec3::new(
                        rng.random_range(1.2..=2.0),
                        rng.random_range(1.2..=2.4),
                        rng.random_range(1.2..=2.0),
                    );
                    push_prop(
                        plan,
                        PropKind::WaterTank,
                        building.position + roof_spot(&mut rng, along, street, l, d, size),
                        building.yaw,
                        size,
                        h,
                        0,
                    );
                }
            }
            // Rooftop stair bulkhead. Size and spot vary, and it appears on
            // just over half the buildings, so the roofline stops repeating.
            if rng.random::<f32>() < 0.55 {
                let size = Vec3::new(
                    rng.random_range(2.6..=3.8),
                    rng.random_range(2.2..=2.9),
                    rng.random_range(2.2..=3.2),
                );
                push_prop(
                    plan,
                    PropKind::StairBox,
                    building.position + roof_spot(&mut rng, along, street, l, d, size),
                    building.yaw,
                    size,
                    h,
                    0,
                );
            }
        }
        FacadeStyle::Factory => {
            // 烟囱: one tall stack through the sawtooth, most sheds.
            if rng.random::<f32>() < 0.8 {
                let u = if rng.random::<bool>() {
                    -l * 0.3
                } else {
                    l * 0.3
                };
                push_prop(
                    plan,
                    PropKind::Chimney,
                    building.position + along * u,
                    building.yaw,
                    Vec3::new(1.1, 7.0, 1.1),
                    h + 1.0,
                    0,
                );
            }
            // Vents between the teeth.
            let vents = rng.random_range(2..=4);
            for i in 0..vents {
                let t = (i as f32 + 0.5) / vents as f32;
                push_prop(
                    plan,
                    PropKind::RoofVent,
                    building.position + along * ((t - 0.5) * l * 0.8),
                    building.yaw,
                    Vec3::new(0.5, 0.9, 0.5),
                    h + 1.2,
                    0,
                );
            }
        }
    }
}

/// One utility pole on an edge side's curb line.
///
/// The pole line is the *only* furniture the rest of the city can hang things
/// from (cross-street cables, long lamp brackets), so it is exposed as data
/// instead of being spawned straight out of a local loop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoleSlot {
    /// Offset from the edge midpoint along the edge (m).
    pub along: f32,
    /// Whether this pole carries the short lamp bracket.
    pub lamp: bool,
    /// Yaw jitter of the pole-top crossarm (rad), added to
    /// `edge_yaw(edge_normal)` by the spawner.
    pub crossarm_jitter: f32,
}

/// The pole line of one edge side, as a pure replayable function.
///
/// Same three draws per pole, in the same order, as the original inline loop,
/// so extracting it moved no pole by a millimetre (the furniture stability
/// test pins that). The cross-street pass in `world::overhead` replays it to
/// hang cables on real poles rather than guessing where the poles are.
pub fn pole_slots(seed: u64, edge_index: u32, side: u32, edge_length: f32) -> Vec<PoleSlot> {
    let mut rng = edge_salt_rng(seed, edge_index, side);
    // Pole slots along the edge, evenly spaced like the tree candidates,
    // with one deterministic micro-jitter so a street of poles does not
    // read as a parade. The jitter draws even when nothing else does, so
    // the stream stays stable under any future rule change.
    let count = ((edge_length / POLE_SPACING).round() as usize).max(2);
    let mut slots = Vec::with_capacity(count);
    for i in 0..count {
        let t = (i as f32 + 0.5) / count as f32;
        let along = (t - 0.5) * edge_length + rng.random_range(-2.5..=2.5);
        // Street lamps are common but not universal on old streets.
        let lamp = rng.random::<f32>() < 0.45;
        let crossarm_jitter = rng.random_range(-CROSSARM_YAW_JITTER..=CROSSARM_YAW_JITTER);
        slots.push(PoleSlot {
            along,
            lamp,
            crossarm_jitter,
        });
    }
    slots
}

/// Places the street furniture of one edge side (R3.11 step 11.5): utility
/// poles, lamp arms with their emissive heads, and the wire spans between
/// consecutive poles.
///
/// `edge_normal` points from the centerline INTO the block (the same
/// convention `place_buildings` receives), so the poles stand on the block
/// side of the curb and the lamp arms reach back over the road. Anchored
/// like the street trees -- one slot per [`POLE_SPACING`] -- but from a
/// dedicated RNG salt, so neither the approved wear sequence nor the tree
/// stream moves by a single draw. Poles always spawn (every street carries
/// lines); only the lamp brackets roll.
#[allow(clippy::too_many_arguments)]
pub fn place_street_furniture(
    plan: &mut ChunkPlan,
    seed: u64,
    edge_index: u32,
    side: u32,
    edge_mid: bevy::math::Vec2,
    edge_dir: bevy::math::Vec2,
    edge_normal: bevy::math::Vec2,
    edge_length: f32,
    curb: f32,
) {
    let street_dir = -edge_normal;
    let stand_line = edge_mid + edge_normal * (curb + POLE_STANDOFF);
    let slots = pole_slots(seed, edge_index, side, edge_length);

    for slot in &slots {
        let position = stand_line + edge_dir * slot.along;
        push_prop(
            plan,
            PropKind::Pole,
            position,
            0.0,
            Vec3::new(0.28, POLE_HEIGHT, 0.28),
            0.0,
            0,
        );
        if slot.lamp {
            // The arm is baked reaching local +x; aim it over the road.
            let arm_yaw = edge_yaw(street_dir);
            push_prop(
                plan,
                PropKind::LampArm,
                position,
                arm_yaw,
                Vec3::ONE,
                LAMP_MOUNT,
                0,
            );
            push_prop(
                plan,
                PropKind::LampHead,
                position,
                arm_yaw,
                Vec3::ONE,
                LAMP_MOUNT,
                0,
            );
        }
        // 横担: the crossarm the wire run meets. Square across the street by
        // default (the wires run along it), with a small per-pole yaw so a
        // street of poles does not read as stamped copies.
        push_prop(
            plan,
            PropKind::CrossArm,
            position,
            edge_yaw(edge_normal) + slot.crossarm_jitter,
            Vec3::ONE,
            CROSSARM_MOUNT,
            0,
        );
    }
    // Wire spans between consecutive poles, three stacked heights each.
    // Each span starts at the earlier pole and scales to the actual gap;
    // the yaw aims local +x along the street.
    for w in 0..slots.len().saturating_sub(1) {
        let span = slots[w + 1].along - slots[w].along;
        if span <= 1.0 {
            continue;
        }
        let start = stand_line + edge_dir * slots[w].along;
        for (rung, height) in WIRE_HEIGHTS.iter().enumerate() {
            push_prop(
                plan,
                PropKind::Wire,
                start,
                edge_yaw(edge_dir),
                Vec3::new(span, span / WIRE_NOMINAL, span / WIRE_NOMINAL),
                *height,
                rung as u8,
            );
        }
    }
}

/// Yaw that turns local +x to `dir` (same convention as the buildings'
/// yaw fix: `Quat::from_rotation_y` maps +x to `(cos y, -sin y)`).
pub(crate) fn edge_yaw(dir: bevy::math::Vec2) -> f32 {
    (-dir.y).atan2(dir.x)
}

/// Street-furniture RNG: a dedicated per-edge salt stream (12000 + side),
/// kept well away from the building/tree salts so no existing sequence
/// shifts by a single draw.
fn edge_salt_rng(seed: u64, edge: u32, side: u32) -> StdRng {
    let salt = 12_000u32.wrapping_add(side);
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)
        ^ (edge as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (salt as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    StdRng::seed_from_u64(mixed)
}

// --- mesh baking -----------------------------------------------------------

/// One axis-aligned box, six quads wound CCW seen from outside.
pub(crate) fn push_box(buf: &mut MeshBuf, min: [f32; 3], max: [f32; 3]) {
    push_box_mapped(buf, min, max, |p| p);
}

/// [`push_box`], rotated about the local +Z axis by `angle` around `pivot`.
///
/// Every prop so far is a stack of axis-aligned boxes; a diagonal strut is the
/// one part that is not, and stretching a unit box cannot make one (the
/// section would shear). Rotating the eight corners keeps `push_box`'s
/// winding: a rotation about +Z is orientation preserving, so each face still
/// points outward.
pub(crate) fn push_box_rot_z(
    buf: &mut MeshBuf,
    min: [f32; 3],
    max: [f32; 3],
    pivot: [f32; 2],
    angle: f32,
) {
    let (s, c) = angle.sin_cos();
    push_box_mapped(buf, min, max, |p| {
        let (dx, dy) = (p[0] - pivot[0], p[1] - pivot[1]);
        [pivot[0] + dx * c - dy * s, pivot[1] + dx * s + dy * c, p[2]]
    });
}

/// The six faces of `min..max` passed through `map` (a rigid motion).
fn push_box_mapped(
    buf: &mut MeshBuf,
    min: [f32; 3],
    max: [f32; 3],
    map: impl Fn([f32; 3]) -> [f32; 3],
) {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    buf.quad(
        map([x1, y0, z1]),
        map([x1, y0, z0]),
        map([x1, y1, z0]),
        map([x1, y1, z1]),
    );
    buf.quad(
        map([x0, y0, z0]),
        map([x0, y0, z1]),
        map([x0, y1, z1]),
        map([x0, y1, z0]),
    );
    buf.quad(
        map([x0, y1, z1]),
        map([x1, y1, z1]),
        map([x1, y1, z0]),
        map([x0, y1, z0]),
    );
    buf.quad(
        map([x0, y0, z0]),
        map([x1, y0, z0]),
        map([x1, y0, z1]),
        map([x0, y0, z1]),
    );
    buf.quad(
        map([x0, y0, z1]),
        map([x1, y0, z1]),
        map([x1, y1, z1]),
        map([x0, y1, z1]),
    );
    buf.quad(
        map([x1, y0, z0]),
        map([x0, y0, z0]),
        map([x0, y1, z0]),
        map([x1, y1, z0]),
    );
}

/// The 雨棚: a tilted canvas slab, wall edge at z = 0, outer edge toward
/// local +Z and 0.21 m lower (a ~12 degree pitch so rain runs off the
/// street side).
///
/// Baked at real size (1.8 m wide, 1.0 m deep) because the tilt does not
/// survive non-uniform scaling; instances scale x only for width.
pub fn bake_awning() -> Mesh {
    const DEPTH: f32 = 1.0;
    const DROP: f32 = 0.21;
    const THICK: f32 = 0.05;
    let (x0, x1) = (-0.9, 0.9);
    // Cross-section in (z, y), CCW seen from +x.
    let cs = [
        [0.0, 0.0],
        [DEPTH, -DROP],
        [DEPTH, -DROP - THICK],
        [0.0, -THICK],
    ];
    let p = |c: [f32; 2], x: f32| [x, c[1], c[0]];
    let mut buf = MeshBuf::new();
    // End caps: the +x cap follows the CCW section, the -x cap reverses it.
    buf.quad(p(cs[0], x1), p(cs[1], x1), p(cs[2], x1), p(cs[3], x1));
    buf.quad(p(cs[3], x0), p(cs[2], x0), p(cs[1], x0), p(cs[0], x0));
    // The four swept faces, one per section edge ci -> ci+1.
    for i in 0..4 {
        let a = cs[i];
        let b = cs[(i + 1) % 4];
        buf.quad(p(a, x1), p(a, x0), p(b, x0), p(b, x1));
    }
    buf.build()
}

/// The 阳台: slab + railing baked as one mesh, wall plane at z = 0, floor
/// level at y = 0 (the slab hangs 0.12 m below its own floor).
pub fn bake_balcony() -> Mesh {
    const RAIL_H: f32 = 0.95;
    let hw = 1.2; // half of the 2.4 m slab
    let mut buf = MeshBuf::new();
    push_box(&mut buf, [-hw, -0.12, 0.0], [hw, 0.0, 1.0]);
    // Railing: front panel on the outer edge, two side panels.
    push_box(&mut buf, [-hw, 0.0, 0.93], [hw, RAIL_H, 1.0]);
    push_box(&mut buf, [-hw, 0.0, 0.0], [-hw + 0.06, RAIL_H, 0.93]);
    push_box(&mut buf, [hw - 0.06, 0.0, 0.0], [hw, RAIL_H, 0.93]);
    buf.build()
}

/// The crossarm: a horizontal beam across the pole top with two insulator
/// pegs where the wire run sits, plus a clamp around the pole. Baked at real
/// size with the beam centre at the origin, because the arm's proportions
/// (beam to peg to clamp) do not survive scaling.
pub fn bake_crossarm() -> Mesh {
    let h = CROSSARM_HALF;
    let mut buf = MeshBuf::new();
    // Beam, slightly wider than deep so it reads as a timber/steel arm.
    push_box(&mut buf, [-h, -0.055, -0.065], [h, 0.055, 0.065]);
    // Insulator pegs: where a third wire would sit either side of the pole.
    for x in [-h * 0.72, h * 0.72] {
        push_box(
            &mut buf,
            [x - 0.045, 0.055, -0.045],
            [x + 0.045, 0.20, 0.045],
        );
    }
    // Clamp: the collar that holds the arm against the pole.
    push_box(&mut buf, [-0.10, -0.20, -0.10], [0.10, 0.055, 0.10]);
    buf.build()
}

/// The lamp arm: a bracket baked reaching toward local +x, mount plate at
/// the origin, head end 1.4 m out and hanging just under the beam.
pub fn bake_lamp_arm() -> Mesh {
    let mut buf = MeshBuf::new();
    // Mount plate against the pole face.
    push_box(&mut buf, [-0.09, -0.09, -0.05], [0.09, 0.09, 0.05]);
    // Arm: a thin horizontal beam reaching +x, sloping slightly down.
    push_box(&mut buf, [0.0, -0.04, -0.04], [1.4, 0.02, 0.04]);
    // Head housing at the far end, overlapping the beam's underside.
    push_box(&mut buf, [1.15, -0.15, -0.12], [1.4, 0.0, 0.12]);
    buf.build()
}

/// The lamp head: the glowing box that sits in the arm's bracket, same
/// origin and orientation as [`bake_lamp_arm`]. Separate mesh so the head
/// carries the emissive material while the arm stays matte.
///
/// It must **protrude** from the bracket: the housing in [`bake_lamp_arm`] is
/// a closed box, so a head that sat inside it (the first version did) was
/// simply occluded -- the one emissive prop in the city never showed a single
/// lit pixel. The lens now hangs 1.8 cm below the housing's underside, which
/// is exactly the face a walker at eye height sees it from.
pub fn bake_lamp_head() -> Mesh {
    let mut buf = MeshBuf::new();
    push_box(&mut buf, [1.19, -0.168, -0.095], [1.36, -0.14, 0.095]);
    buf.build()
}

/// One catenary wire span, baked from x = 0 to x = 1, sagging along a
/// parabola pinned to y = 0 at both ends with an absolute `sag` dip at the
/// middle.
///
/// Instances translate to the *earlier* pole of the span, scale x by the
/// real pole-to-pole distance and y by `span / nominal`: the dip then
/// grows with the span the way a real catenary does, and the cable keeps
/// its baked radius on the z axis.
///
/// Shared by the three-wire run along a street ([`bake_wire`]) and the
/// cross-street drop lines in `world::overhead` ([`bake_cross_wire`]): the
/// two differ only in sag, radius and nominal span, which is what the
/// parameters are.
pub(crate) fn bake_catenary(sag: f32, radius: f32) -> Mesh {
    const SEGMENTS: usize = 8;
    let mut buf = MeshBuf::new();
    let dip = |x: f32| 4.0 * sag * x * (1.0 - x);
    // A thin tube with a diamond section: at a couple of centimetres nobody
    // can tell it from a circle, and it quarters the vertex count.
    let ring = |x: f32| {
        let r = radius;
        let y = -dip(x);
        [[x, y + r, 0.0], [x, y, r], [x, y - r, 0.0], [x, y, -r]]
    };
    for s in 0..SEGMENTS {
        let x0 = s as f32 / SEGMENTS as f32;
        let x1 = (s + 1) as f32 / SEGMENTS as f32;
        let a = ring(x0);
        let b = ring(x1);
        for i in 0..4 {
            let j = (i + 1) % 4;
            // Side quads, CCW seen from outside.
            buf.quad(a[i], a[j], b[j], b[i]);
        }
    }
    // Tiny end caps (the wire meets a pole at each end anyway).
    let a = ring(0.0);
    let b = ring(1.0);
    buf.tri(a[0], a[2], a[1]);
    buf.tri(a[0], a[3], a[2]);
    buf.tri(b[0], b[1], b[2]);
    buf.tri(b[0], b[2], b[3]);
    buf.build()
}

/// The three-wire run along a street, at [`WIRE_SAG`] / [`WIRE_RADIUS`].
pub fn bake_wire() -> Mesh {
    bake_catenary(WIRE_SAG, WIRE_RADIUS)
}

/// Places the street furniture of one edge side (R3.11 step 11.5): utility
/// poles, lamp arms with their emissive heads, and the wire spans between
/// consecutive poles.
///
/// `edge_normal` points from the centerline INTO the block (the same
/// convention `place_buildings` receives), so the poles stand on the block
/// side of the curb and the lamp arms reach back over the road. Anchored
/// like the street trees -- one slot per [`POLE_SPACING`] -- but from a
/// dedicated RNG salt, so neither the approved wear sequence nor the tree
/// stream moves by a single draw. Poles always spawn (every street carries
/// lines); only the lamp brackets roll.
#[cfg(test)]
mod tests {
    use super::*;

    use crate::world::layout_graph::BlockType;

    /// A synthetic building at a fixed spot; `street` points toward -z, so
    /// the facade faces the -z side and the yaw-flip branch is exercised.
    fn building(style: FacadeStyle, offset: f32, size: Vec3, wear: f32) -> PlacedObject {
        PlacedObject {
            kind: PlacedKind::Building(match style {
                FacadeStyle::OldCity => BlockType::OldCity,
                FacadeStyle::Apartment => BlockType::Apartment,
                FacadeStyle::Factory => BlockType::Factory,
            }),
            position: bevy::math::Vec2::new(100.0 + offset, 200.0),
            yaw: 0.0,
            size,
            footprint_half: bevy::math::Vec2::new(size.x * 0.5, size.z * 0.5),
            wear,
            variant: 0,
            tree: None,
            base: 0.0,
        }
    }

    const STREET: bevy::math::Vec2 = bevy::math::Vec2::new(0.0, -1.0);

    fn props_of(style: FacadeStyle, size: Vec3, wear: f32, offset: f32) -> Vec<PlacedObject> {
        let mut plan = ChunkPlan::default();
        let b = building(style, offset, size, wear);
        place_building_props(&mut plan, &b, style, STREET);
        plan.objects
    }

    #[test]
    fn props_are_position_keyed_and_stable() {
        let reference = props_of(
            FacadeStyle::Apartment,
            Vec3::new(33.0, 18.0, 13.0),
            0.4,
            0.0,
        );
        assert!(
            !reference.is_empty(),
            "an 33 m six-floor slab must grow some props"
        );
        let again = props_of(
            FacadeStyle::Apartment,
            Vec3::new(33.0, 18.0, 13.0),
            0.4,
            0.0,
        );
        assert_eq!(reference, again, "the same building must dress identically");
        // Different positions dress differently: the rolls are position
        // keyed, so 30 shifted copies must not all collapse onto one answer.
        let differing = (0..30)
            .filter(|i| {
                props_of(
                    FacadeStyle::Apartment,
                    Vec3::new(33.0, 18.0, 13.0),
                    0.4,
                    *i as f32,
                ) != reference
            })
            .count();
        assert!(
            differing >= 25,
            "position keying collapsed: only {differing}/30 differ"
        );
    }

    #[test]
    fn facade_props_hug_the_front_plane() {
        for (style, size) in [
            (FacadeStyle::OldCity, Vec3::new(6.0, 6.0, 5.0)),
            (FacadeStyle::Apartment, Vec3::new(33.0, 18.0, 13.0)),
            (FacadeStyle::Factory, Vec3::new(50.0, 24.0, 50.0)),
        ] {
            for offset in [0.0, 3.7, 11.2] {
                let center = building(style, offset, size, 0.5).position;
                for p in props_of(style, size, 0.5, offset) {
                    let PlacedKind::Prop(kind) = p.kind else {
                        continue;
                    };
                    let rel = p.position - center;
                    // + means toward the street; `along` is the facade run.
                    let out = rel.dot(STREET);
                    let along = rel.dot(bevy::math::Vec2::new(STREET.y, -STREET.x));
                    let roof_kind = matches!(
                        kind,
                        PropKind::WaterTank | PropKind::StairBox | PropKind::RoofVent
                    );
                    match kind {
                        PropKind::Awning | PropKind::Balcony => {
                            assert!(
                                (out - size.z * 0.5).abs() < 1e-4,
                                "{kind:?} must sit on the facade plane, got {out}"
                            );
                        }
                        PropKind::AcUnit => {
                            let want = size.z * 0.5 + AC_DEPTH * 0.5;
                            assert!(
                                (out - want).abs() < 1e-4,
                                "{kind:?} must protrude {want} m, got {out}"
                            );
                        }
                        PropKind::Downspout => {
                            assert!((out - size.z * 0.5).abs() < 1e-4);
                            assert!(along.abs() <= size.x * 0.5 + 1e-4);
                        }
                        PropKind::Chimney => assert!(out.abs() <= size.z * 0.4 + 1e-4),
                        _ => {}
                    }
                    let along_limit = if roof_kind || kind == PropKind::Chimney {
                        size.x * 0.45
                    } else {
                        size.x * 0.5
                    };
                    assert!(
                        along.abs() <= along_limit + 1e-4,
                        "{kind:?} drifted {along} m along the facade (limit {along_limit})"
                    );
                }
            }
        }
    }

    #[test]
    fn roof_props_poke_above_the_roofline() {
        for (style, size) in [
            (FacadeStyle::OldCity, Vec3::new(6.0, 6.0, 5.0)),
            (FacadeStyle::Apartment, Vec3::new(33.0, 18.0, 13.0)),
            (FacadeStyle::Factory, Vec3::new(50.0, 24.0, 50.0)),
        ] {
            for p in props_of(style, size, 0.5, 2.3) {
                let PlacedKind::Prop(kind) = p.kind else {
                    continue;
                };
                let top = p.base + p.size.y;
                match kind {
                    PropKind::WaterTank | PropKind::StairBox => {
                        assert!(
                            (p.base - size.y).abs() < 1e-4,
                            "{kind:?} must stand on the roof deck"
                        );
                        assert!(top > size.y, "{kind:?} must rise above the parapet");
                    }
                    PropKind::Chimney => {
                        assert!(top >= size.y + 0.5, "chimney must clear the roofline");
                        assert!(p.base <= size.y + 1.2, "chimney base belongs at the roof");
                    }
                    PropKind::RoofVent => {
                        assert!(p.base >= size.y, "vent sits on the roof");
                        assert!(top <= size.y + 2.3, "vent stays under the tooth peaks");
                    }
                    _ => {
                        assert!(
                            p.base >= 0.0 && top <= size.y + 0.1,
                            "{kind:?} belongs on the wall"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn styles_get_their_signature_props() {
        let mut balcony = 0;
        let mut tank = 0;
        let mut stair = 0;
        // Roof clutter must not repeat one identical box along a terrace row
        // (that read as a regular sawtooth on the skyline).
        let mut tank_widths = std::collections::HashSet::new();
        let mut stair_heights = std::collections::HashSet::new();
        for i in 0..60 {
            for p in props_of(
                FacadeStyle::Apartment,
                Vec3::new(33.0, 18.0, 13.0),
                0.5,
                i as f32 * 1.7,
            ) {
                match p.kind {
                    PlacedKind::Prop(PropKind::Balcony) => balcony += 1,
                    PlacedKind::Prop(PropKind::WaterTank) => {
                        tank += 1;
                        tank_widths.insert((p.size.x * 10.0).round() as i32);
                    }
                    PlacedKind::Prop(PropKind::StairBox) => {
                        stair += 1;
                        stair_heights.insert((p.size.y * 10.0).round() as i32);
                    }
                    _ => {}
                }
            }
        }
        assert!(balcony > 100, "apartments need balcony rows, got {balcony}");
        assert!(tank >= 25, "apartments need roof tanks, got {tank}");
        assert!(stair >= 30, "apartments need stair bulkheads, got {stair}");
        assert!(
            tank_widths.len() >= 4,
            "roof tanks repeat {} sizes across buildings",
            tank_widths.len()
        );
        assert!(
            stair_heights.len() >= 3,
            "stair bulkheads repeat {} heights across buildings",
            stair_heights.len()
        );

        // Old city never grows apartment clutter: gable roofs have no deck.
        for i in 0..40 {
            for p in props_of(
                FacadeStyle::OldCity,
                Vec3::new(6.0, 6.0, 5.0),
                0.5,
                i as f32,
            ) {
                assert!(!matches!(
                    p.kind,
                    PlacedKind::Prop(PropKind::Balcony)
                        | PlacedKind::Prop(PropKind::WaterTank)
                        | PlacedKind::Prop(PropKind::StairBox)
                        | PlacedKind::Prop(PropKind::RoofVent)
                ));
            }
        }

        // Factory sheds grow a chimney most of the time (0.8 roll).
        let chimneys = (0..40)
            .filter(|i| {
                props_of(
                    FacadeStyle::Factory,
                    Vec3::new(50.0, 24.0, 50.0),
                    0.5,
                    *i as f32,
                )
                .iter()
                .any(|p| matches!(p.kind, PlacedKind::Prop(PropKind::Chimney)))
            })
            .count();
        assert!(
            (26..=40).contains(&chimneys),
            "chimney fraction {chimneys}/40 is off the 0.8 roll"
        );
    }

    #[test]
    fn prop_counts_stay_in_budget() {
        let oc = props_of(FacadeStyle::OldCity, Vec3::new(6.0, 6.0, 5.0), 0.9, 0.0).len();
        assert!(oc <= 9, "old city dressing {oc} exceeds the module budget");
        let apt = props_of(
            FacadeStyle::Apartment,
            Vec3::new(39.6, 18.0, 14.0),
            0.9,
            0.0,
        )
        .len();
        assert!(
            apt <= 45,
            "apartment dressing {apt} exceeds the slab budget"
        );
        let fac = props_of(FacadeStyle::Factory, Vec3::new(50.0, 24.0, 50.0), 0.9, 0.0).len();
        assert!(fac <= 8, "factory dressing {fac} exceeds the shed budget");
    }

    /// Extracts the triangle soup of a baked mesh.
    fn triangles(mesh: &Mesh) -> Vec<([f32; 3], [f32; 3], [f32; 3])> {
        let positions: Vec<[f32; 3]> = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .expect("positions")
            .as_float3()
            .expect("f32")
            .to_vec();
        let indices = match mesh.indices().expect("indices") {
            bevy::mesh::Indices::U16(i) => i.iter().map(|&v| v as u32).collect::<Vec<_>>(),
            bevy::mesh::Indices::U32(i) => i.to_vec(),
        };
        indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                (
                    positions[t[0] as usize],
                    positions[t[1] as usize],
                    positions[t[2] as usize],
                )
            })
            .collect()
    }

    /// Every triangle's CCW normal must point away from the mesh centroid:
    /// one inverted face shows in-game as a hole no other test can see.
    fn assert_faces_outward(mesh: &Mesh, name: &str) {
        let tris = triangles(mesh);
        assert!(!tris.is_empty(), "{name} baked empty");
        let mut centroid = [0.0f32; 3];
        let mut count = 0;
        for (a, b, c) in &tris {
            for axis in 0..3 {
                centroid[axis] += (a[axis] + b[axis] + c[axis]) / 3.0;
            }
            count += 1;
        }
        for value in &mut centroid {
            *value /= count as f32;
        }
        for (i, (a, b, c)) in tris.iter().enumerate() {
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let n = [
                ab[1] * ac[2] - ab[2] * ac[1],
                ab[2] * ac[0] - ab[0] * ac[2],
                ab[0] * ac[1] - ab[1] * ac[0],
            ];
            let to_face = [
                (a[0] + b[0] + c[0]) / 3.0 - centroid[0],
                (a[1] + b[1] + c[1]) / 3.0 - centroid[1],
                (a[2] + b[2] + c[2]) / 3.0 - centroid[2],
            ];
            let dot = n[0] * to_face[0] + n[1] * to_face[1] + n[2] * to_face[2];
            assert!(dot > 0.0, "{name} triangle {i} faces inward (dot {dot})");
        }
    }

    #[test]
    fn baked_prop_meshes_face_outward() {
        // The awning is one convex slab, so the whole-mesh centroid test
        // applies directly.
        assert_faces_outward(&bake_awning(), "awning");
        // The balcony is a compound of boxes: a whole-mesh centroid is not
        // inside any one box, so the heuristic only makes sense per box.
        // `push_box` is the thing that must be right -- every box it emits
        // has to face away from its own centre.
        let mut buf = MeshBuf::new();
        push_box(&mut buf, [-1.0, -2.0, -3.0], [0.5, 0.7, 1.5]);
        assert_faces_outward(&buf.build(), "push_box");
        // And the assembled balcony is non-empty geometry.
        assert!(
            !triangles(&bake_balcony()).is_empty(),
            "balcony baked empty"
        );
    }

    #[test]
    fn awning_tilts_down_toward_the_street() {
        let tris = triangles(&bake_awning());
        let mut wall_y = 0.0;
        let mut wall_n = 0;
        let mut outer_y = 0.0;
        let mut outer_n = 0;
        for (a, b, c) in &tris {
            for p in [a, b, c] {
                if p[2].abs() < 1e-5 {
                    wall_y += p[1];
                    wall_n += 1;
                } else if p[2] > 0.9 {
                    outer_y += p[1];
                    outer_n += 1;
                }
            }
        }
        assert!(
            wall_n > 0 && outer_n > 0,
            "awning lost its wall or outer edge"
        );
        let outer = outer_y / outer_n as f32;
        let wall = wall_y / wall_n as f32;
        assert!(
            outer < wall - 0.15,
            "the outer edge must hang clearly lower than the wall edge"
        );
    }

    // --- street furniture (11.5) ---

    const EDGE_MID: bevy::math::Vec2 = bevy::math::Vec2::new(500.0, 500.0);

    /// Furniture for one synthetic edge side: dir = +x, curb at 4 m, the
    /// into-block normal flipped per side like `plan_chunk` passes it.
    fn furniture(side: u32, length: f32) -> Vec<PlacedObject> {
        let mut plan = ChunkPlan::default();
        place_street_furniture(
            &mut plan,
            7,
            3,
            side,
            EDGE_MID,
            bevy::math::Vec2::new(1.0, 0.0),
            bevy::math::Vec2::new(0.0, if side == 0 { 1.0 } else { -1.0 }),
            length,
            4.0,
        );
        plan.objects
    }

    #[test]
    fn poles_stand_at_the_curb_on_their_own_side() {
        for side in [0u32, 1] {
            let objects = furniture(side, 120.0);
            let poles: Vec<&PlacedObject> = objects
                .iter()
                .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Pole)))
                .collect();
            assert_eq!(
                poles.len(),
                4,
                "a 120 m edge carries round(120/30) = 4 poles"
            );
            let sign = if side == 0 { 1.0 } else { -1.0 };
            for p in &poles {
                let rel = p.position - EDGE_MID;
                assert!(rel.x.abs() < 60.0, "pole wanders off the edge");
                assert!(
                    (rel.y - sign * (4.0 + POLE_STANDOFF)).abs() < 1e-4,
                    "pole must stand at curb + standoff on side {side}"
                );
                assert!(
                    (p.size.y - POLE_HEIGHT).abs() < 1e-4,
                    "pole is {POLE_HEIGHT} m"
                );
            }
        }
    }

    #[test]
    fn lamp_arms_reach_over_the_road() {
        for side in [0u32, 1] {
            let objects = furniture(side, 120.0);
            let arms: Vec<&PlacedObject> = objects
                .iter()
                .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::LampArm)))
                .collect();
            assert!(
                !arms.is_empty() && arms.len() < 5,
                "lamps are a 0.45 roll per pole, got {}",
                arms.len()
            );
            let street = bevy::math::Vec2::new(0.0, if side == 0 { -1.0 } else { 1.0 });
            for arm in &arms {
                // Local +x (the arm's reach) maps to (cos y, -sin y).
                let reach = bevy::math::Vec2::new(arm.yaw.cos(), -arm.yaw.sin());
                assert!(
                    reach.dot(street) > 0.999,
                    "the arm must reach toward the centerline"
                );
                assert!(
                    objects
                        .iter()
                        .any(|o| matches!(o.kind, PlacedKind::Prop(PropKind::LampHead))
                            && o.position == arm.position
                            && o.base == arm.base),
                    "every arm carries its glowing head"
                );
            }
        }
    }

    #[test]
    fn every_pole_carries_a_crossarm_at_the_wire_height() {
        let objects = furniture(0, 120.0);
        let poles = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Pole)))
            .count();
        let arms: Vec<&PlacedObject> = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::CrossArm)))
            .collect();
        assert_eq!(arms.len(), poles, "one crossarm per pole");

        // The run is along +x here, so a crossarm is square across it (local
        // +x maps to the +y normal, `edge_yaw(0,1)`), give or take the
        // per-pole jitter.
        let square = edge_yaw(bevy::math::Vec2::new(0.0, 1.0));
        let mut yaws = std::collections::HashSet::new();
        for arm in &arms {
            assert!(
                (arm.base - CROSSARM_MOUNT).abs() < 1e-6,
                "crossarm at {} m, expected {CROSSARM_MOUNT}",
                arm.base
            );
            let jitter = arm.yaw - square;
            assert!(
                jitter.abs() <= CROSSARM_YAW_JITTER + 1e-5,
                "crossarm yaw {jitter} rad exceeds the {CROSSARM_YAW_JITTER} rad jitter"
            );
            yaws.insert((arm.yaw * 1000.0).round() as i32);
        }
        // A stamped line of identical arms is exactly what the jitter is
        // there to break, so neighbouring poles must differ.
        assert!(
            yaws.len() >= 3,
            "crossarms share only {} distinct yaws across {poles} poles",
            yaws.len()
        );
    }

    #[test]
    fn wires_bridge_consecutive_poles_at_stacked_heights() {
        let objects = furniture(0, 120.0);
        let mut alongs: Vec<f32> = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Pole)))
            .map(|o| o.position.x - EDGE_MID.x)
            .collect();
        alongs.sort_by(f32::total_cmp);
        let wires: Vec<&PlacedObject> = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Wire)))
            .collect();
        assert_eq!(
            wires.len(),
            (alongs.len() - 1) * WIRE_HEIGHTS.len(),
            "three stacked wires per span"
        );
        for w in 0..alongs.len() - 1 {
            let span = alongs[w + 1] - alongs[w];
            for &height in WIRE_HEIGHTS.iter() {
                let wire = wires
                    .iter()
                    .find(|o| o.base == height && o.position.x - EDGE_MID.x == alongs[w])
                    .unwrap_or_else(|| panic!("no wire at {height} m from pole {w}"));
                assert!((wire.size.x - span).abs() < 1e-4, "wire spans the real gap");
                // The y and z scales carry the span-proportional sag and
                // keep the cable near-round.
                assert!((wire.size.y - span / WIRE_NOMINAL).abs() < 1e-4);
                assert!((wire.size.z - span / WIRE_NOMINAL).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn furniture_streams_are_stable_and_edge_keyed() {
        let a = furniture(0, 120.0);
        let b = furniture(0, 120.0);
        assert_eq!(a, b, "the same edge side must wire identically every plan");
        // A different edge index re-rolls the jitter (30 samples, at least
        // one must differ -- the streams are per-edge).
        let differing = (0..30u32)
            .filter(|e| {
                let mut plan = ChunkPlan::default();
                place_street_furniture(
                    &mut plan,
                    7,
                    *e,
                    0,
                    EDGE_MID,
                    bevy::math::Vec2::new(1.0, 0.0),
                    bevy::math::Vec2::new(0.0, 1.0),
                    120.0,
                    4.0,
                );
                plan.objects != a
            })
            .count();
        assert!(
            differing >= 25,
            "edge keying collapsed: {differing}/30 differ"
        );
    }

    #[test]
    fn short_edges_still_get_two_poles_and_wires() {
        let objects = furniture(1, 20.0);
        let poles = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Pole)))
            .count();
        assert_eq!(poles, 2, "even a 20 m alley carries a pole pair");
        let wires = objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(PropKind::Wire)))
            .count();
        assert_eq!(wires, WIRE_HEIGHTS.len(), "one span between the pair");
    }

    #[test]
    fn lamp_and_wire_meshes_are_sound() {
        // Arm and head are compounds of boxes; `push_box` winding is
        // validated on its own above, so these only need to be non-empty.
        assert!(
            !triangles(&bake_lamp_arm()).is_empty(),
            "lamp arm baked empty"
        );
        assert!(
            !triangles(&bake_lamp_head()).is_empty(),
            "lamp head baked empty"
        );
        // The wire is an open tube: unit span, endpoints level, middle
        // sagged by exactly WIRE_SAG, nothing above the mount plane.
        let tris = triangles(&bake_wire());
        assert!(!tris.is_empty(), "wire baked empty");
        let (mut min_y, mut max_y, mut min_x, mut max_x) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for (a, b, c) in &tris {
            for p in [a, b, c] {
                min_y = min_y.min(p[1]);
                max_y = max_y.max(p[1]);
                min_x = min_x.min(p[0]);
                max_x = max_x.max(p[0]);
            }
        }
        assert!(
            min_x.abs() < 1e-6 && (max_x - 1.0).abs() < 1e-6,
            "unit span"
        );
        // The tube's lowest point is the sag plus its own radius.
        assert!(
            (min_y + WIRE_SAG + WIRE_RADIUS).abs() < 1e-4,
            "sag baked at the middle"
        );
        assert!(
            max_y <= WIRE_RADIUS + 1e-5,
            "the wire hangs from its mount height, not above it"
        );
    }
}
