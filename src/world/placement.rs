//! Placement (R3.1/R3.2): buildings and trees along street edges, ported from
//! the original `object_placer.gd` (ADR-0004 §4/§5).
//!
//! Placement is **pure data**: [`plan_chunk`] turns one 50 m chunk into a list
//! of [`PlacedObject`]s (kind, transform, footprint) and never touches the ECS.
//! The streaming system (`chunks.rs`) turns that list into meshes, so the same
//! plan can be asserted in unit tests without a window or a GPU.
//!
//! Rules carried over unchanged:
//! - buildings sit on **both** sides of every street segment, inset by
//!   `street_width / 2 + setback + depth / 2` from the centerline;
//! - a footprint is dropped unless it stays out of every street corridor,
//!   inside its own block polygon, and inside the map;
//! - each edge/side pair draws from its own deterministic RNG stream
//!   (`hash_seed(seed, edge, side)`), so one chunk's content never depends on
//!   load order;
//! - every object is placed by exactly one chunk, decided by its anchor point
//!   (edge midpoint for street furniture, block center for block interiors),
//!   the original's `_is_point_in_chunk` de-duplication.
//!
//! Deviations from the original (recorded in `dev-plan.md`):
//! - the map edge is the **scenery band** rather than an `is_outer` block, so
//!   the ring road's outer side has no block and therefore no buildings;
//! - only three tree assets exist (`aspen_1`, `tree_2`, `tree_3`); the plan
//!   samples from whatever is available instead of assuming five.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::world::facade::{FacadeStyle, VARIANT_COUNT, snap_to_bay};
use crate::world::geometry::{
    convex_hull, dist_point_to_segment, dist_segment_to_segment, oriented_rect_inside_polygon,
    point_in_polygon,
};
use crate::world::layout_graph::{BlockType, LayoutGraph};
use crate::world::props::PropKind;
use crate::world::{CELL_SIZE, CHUNK_SIZE, GRID_CELLS};

/// Street trees are spaced on this grid along an edge (m).
pub const TREE_SPACING: f32 = 8.0;
/// Tree scale range (the original draws one factor for x/y/z).
pub const TREE_SCALE_MIN: f32 = 0.8;
pub const TREE_SCALE_MAX: f32 = 1.2;

/// Street width used for the road-corridor rejection test (m). The original
/// hard-codes 8 m here even though per-street widths vary by ±2 m; the
/// corridor is only a cheap first filter (the block polygon is the real
/// bound), so the constant is kept for portability.
pub const CORRIDOR_WIDTH: f32 = 8.0;

/// Buildings of one block type (ADR-0004 §4 parameter table).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BuildingParams {
    /// Extent along the street (m): `[min, max]`.
    pub length: (f32, f32),
    /// Extent away from the street (m): `[min, max]`.
    pub depth: (f32, f32),
    /// Height (m): `[min, max]`.
    pub height: (f32, f32),
    /// Gap between neighbours along the edge (m).
    pub spacing: f32,
    /// Distance from the curb to the facade (m).
    pub setback: f32,
    /// Street-tree probability inside the setback strip (0 for walkable
    /// block types).
    pub street_tree_probability: f32,
}

impl BuildingParams {
    /// Parameters for a block type, or `None` for walkable types (PLAZA,
    /// FOREST) that carry no buildings.
    pub fn of(kind: BlockType) -> Option<Self> {
        let params = match kind {
            BlockType::OldCity => BuildingParams {
                length: (4.0, 6.0),
                depth: (4.0, 6.0),
                height: (3.0, 6.0),
                spacing: 0.1,
                setback: 1.0,
                street_tree_probability: 0.2,
            },
            BlockType::Apartment => BuildingParams {
                length: (25.0, 40.0),
                depth: (12.0, 14.0),
                height: (18.0, 18.0),
                spacing: 0.0,
                setback: 5.0,
                street_tree_probability: 0.6,
            },
            BlockType::Factory => BuildingParams {
                length: (50.0, 50.0),
                depth: (50.0, 50.0),
                height: (24.0, 24.0),
                spacing: 0.0,
                setback: 10.0,
                street_tree_probability: 0.8,
            },
            // 城中村 self-built houses (2026-11 field visit): 3 storeys packed
            // against the lane. Length 4-7 m and depth 5-8 m read as one
            // house per lane-facing plot; spacing 0 keeps neighbours touching
            // (贴线而建), the 0.25 m setback is the drainage strip the walls
            // actually keep from the curb. Heights snap to the OldCity-style
            // 3.0 m floor, and 8.5-10.0 rounds to exactly 3 storeys on every
            // building — the "self-built houses are all ~3 floors" rule.
            // Street trees: none, a lane has no verge for them.
            BlockType::UrbanVillage => BuildingParams {
                length: (4.0, 7.0),
                depth: (5.0, 8.0),
                height: (8.5, 10.0),
                spacing: 0.0,
                setback: 0.25,
                street_tree_probability: 0.0,
            },
            BlockType::Plaza | BlockType::Forest => return None,
        };
        Some(params)
    }
}

/// Style colours for the BoxMesh placeholders (ADR-0004: silhouettes read at
/// dusk, so the palette stays muted).
#[derive(Clone, Copy, Debug)]
pub struct StylePalette {
    pub old_city: [f32; 3],
    pub apartment: [f32; 3],
    pub factory: [f32; 3],
    /// 城中村: warm beige plaster and terracotta tile patches, a notch
    /// warmer than the old city's grey brick so the district reads from a
    /// distance.
    pub urban_village: [f32; 3],
    pub plaza: [f32; 3],
}

impl Default for StylePalette {
    fn default() -> Self {
        StylePalette {
            old_city: [0.80, 0.60, 0.30],
            apartment: [0.50, 0.50, 0.60],
            factory: [0.40, 0.25, 0.25],
            urban_village: [0.78, 0.68, 0.54],
            plaza: [0.70, 0.70, 0.65],
        }
    }
}

/// What the player sees standing at a planned position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlacedKind {
    /// Box placeholder in place of a Blender module (R3.6 adds wear).
    Building(BlockType),
    /// A building mass in the scenery band behind the ring road
    /// (`world::band`): the same cube, facade and roof as a [`Self::Building`],
    /// but it stands outside every block, on ground the collision ring has
    /// already sealed. Kept as its own kind because it is the one building
    /// that legitimately stands inside a street corridor -- the ring road's
    /// outer half, where nobody can walk.
    SceneryBuilding(BlockType),
    /// Street tree inside a block's setback strip.
    StreetTree,
    /// Tree inside a FOREST block.
    ForestTree,
    /// Tree on a PLAZA block.
    PlazaTree,
    /// Tree in the scenery band: the alleys between the ring road's facades,
    /// and the grove in each map corner.
    SceneryTree,
    /// The plaza's paved pad.
    PlazaPad,
    /// A facade, roof or street prop (R3.11 steps 11.4-11.5).
    Prop(PropKind),
}

/// A bare tree asset: which mesh, how big, which way it faces.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreeInstance {
    /// Index into the available asset list (stable within one plan).
    pub asset: u8,
    /// Uniform scale (0.8-1.2).
    pub scale: f32,
    /// Y rotation (radians, 0..2π).
    pub yaw: f32,
}

/// One placed object: transform + footprint, no ECS entity.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedObject {
    pub kind: PlacedKind,
    /// Ground-plane center (x, z).
    pub position: bevy::math::Vec2,
    /// Y rotation (radians). Buildings face the street; trees are random.
    pub yaw: f32,
    /// Box size `(length, height, depth)` for buildings, `(width, height,
    /// depth)` for the plaza pad; unused for trees (they carry [`TreeInstance`]).
    pub size: bevy::math::Vec3,
    /// Terrain footprint half extents (x, z), used by tests to re-check the
    /// street/block/bounds rules.
    pub footprint_half: bevy::math::Vec2,
    /// Deterministic wear level 0.0 (fresh) .. 1.0 (heavily worn), baked from
    /// the placement RNG at plan time so the same seed always produces the
    /// same weathering per building (R3.6). Unused for trees.
    pub wear: f32,
    /// Which facade pattern variant this building draws (R3.11), so two
    /// neighbours rarely share the same 12 m tile. Derived from the building's
    /// own position rather than from the RNG: taking another draw from the
    /// per-edge stream would shift every wear value downstream of it, and the
    /// existing weathering is already approved.
    ///
    /// Props reuse the slot as a material selector: 0 = standard finish,
    /// 1 = rusted (or the second awning canvas).
    pub variant: u8,
    /// Tree variation, `None` for buildings and the plaza pad.
    pub tree: Option<TreeInstance>,
    /// Height of the object's bottom above the ground (m). Buildings, trees
    /// and the plaza pad start at 0; props hang at their mount height. The
    /// spawn side centres unit meshes at `base + size.y/2` and hangs baked
    /// meshes (whose origin sits at the mount point) at `base` directly.
    pub base: f32,
}

/// Everything one chunk displays, in spawn order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChunkPlan {
    /// True once every tree asset finished loading when this plan was made.
    /// A plan built earlier has no trees at all and must be re-planned.
    pub trees_ready: bool,
    pub objects: Vec<PlacedObject>,
}

impl ChunkPlan {
    /// Building count (diagnostics/tests).
    pub fn building_count(&self) -> usize {
        self.objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Building(_)))
            .count()
    }

    /// Tree count, all reasons combined.
    pub fn tree_count(&self) -> usize {
        self.objects.iter().filter(|o| o.tree.is_some()).count()
    }

    /// Prop count, facade/roof/street combined (diagnostics/tests).
    pub fn prop_count(&self) -> usize {
        self.objects
            .iter()
            .filter(|o| matches!(o.kind, PlacedKind::Prop(_)))
            .count()
    }
}

/// Determines which chunk owns an anchor point (the original's per-object
/// de-duplication): a point on a chunk seam belongs to the higher index.
pub fn anchor_chunk(anchor: bevy::math::Vec2) -> (i32, i32) {
    (
        (anchor.x / CHUNK_SIZE).floor() as i32,
        (anchor.y / CHUNK_SIZE).floor() as i32,
    )
}

/// Per-edge/per-side RNG stream (the original's `hash_seed`): a fixed, odd
/// multiplier keeps neighbouring edge indices from drawing correlated
/// streams, and the chunk is independent of load order.
fn edge_rng(seed: u64, edge: u32, salt: u32) -> StdRng {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17)
        ^ (edge as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (salt as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    StdRng::seed_from_u64(mixed)
}

/// Plans every object whose anchor lies in chunk `(cx, cz)`.
///
/// `tree_assets` is the number of tree meshes available (0 disables trees and
/// leaves `trees_ready == false`, so the caller re-plans once loading ends).
pub fn plan_chunk(graph: &LayoutGraph, seed: u64, chunk: (i32, i32), tree_assets: u8) -> ChunkPlan {
    let mut plan = ChunkPlan {
        trees_ready: tree_assets > 0,
        objects: Vec::new(),
    };

    for (edge_index, edge) in graph.edges.iter().enumerate() {
        let a = graph.nodes[edge.from as usize].pos;
        let b = graph.nodes[edge.to as usize].pos;
        let mid = (a + b) * 0.5;
        if anchor_chunk(mid) != chunk {
            continue;
        }
        let dir = (b - a).normalize_or_zero();
        if dir == bevy::math::Vec2::ZERO {
            continue;
        }
        let normal = bevy::math::Vec2::new(-dir.y, dir.x);
        let length = (b - a).length();

        // ADR-0008: tier-1 lanes carry no buildings, trees, poles or
        // overhead spans of their own. Their frontages are placed below as
        // part of the parent block's sub-block pass; the lane itself is
        // walkable negative space between the plots.
        if edge.kind == crate::world::layout_graph::EdgeKind::Lane {
            continue;
        }

        for (side, block_id) in [(0u32, edge.left_block), (1, edge.right_block)] {
            let Some(block_id) = block_id else {
                continue; // Scenery band: nothing to build against.
            };
            let Some(block) = graph.blocks.get(block_id as usize) else {
                continue;
            };
            if block.polygon.len() < 3 {
                continue;
            }
            // Left of `from -> to` is `normal`; right is `-normal`.
            let sign = if side == 0 { 1.0 } else { -1.0 };
            let Some(params) = BuildingParams::of(block.kind) else {
                continue;
            };
            if std::env::var_os("HU_TRACE_PLACEMENT").is_some() {
                eprintln!(
                    "plan: edge {edge_index} side {side} block {block_id} kind {:?} len {length:.0} sign {sign} normal {:?}",
                    block.kind,
                    normal * sign
                );
            }
            // One run per (edge, side). The old three-way split existed to keep
            // the depth probe off the block's mitred corners, but it also capped
            // every frontage at ~15 m, which is shorter than an apartment's
            // 25-40 m table length: every apartment and factory row failed and
            // fell back to 3-6 m modules, and no building in the city ever
            // showed its intended footprint. `place_buildings` now measures the
            // block's own usable frontage (`usable_frontage`), so the corners
            // are handled where they are understood.
            //
            // The curb sits at this street's own half-width from the
            // centerline; buildings and trees offset from the curb, not from
            // a global corridor (streets vary 6-10 m).
            let curb = edge.width * 0.5;
            // ADR-0008: a subdivided block is built entirely by the plot
            // pass below (each plot is one lot, filled from its lane
            // frontage to its back edge), so placing a whole-block street
            // row here too would stack two rows in the outer strip.
            // Empty children = the pre-ADR-0008 behaviour, bit-exact.
            // A subdivided block is furnished entirely by its plots (below);
            // a whole-block row here would double-stack the outer strip.
            if block.children.is_empty() {
                let mut rng = edge_rng(seed, edge_index as u32, side * 16);
                place_buildings(
                    graph,
                    &mut plan,
                    &mut rng,
                    block,
                    &block.polygon,
                    &params,
                    (a + b) * 0.5,
                    dir,
                    normal * sign,
                    length,
                    curb,
                    1.0,
                );
            }
            let mut rng = edge_rng(seed, edge_index as u32, 1000 + side * 16);
            place_street_trees(
                &mut plan,
                &mut rng,
                &params,
                (a + b) * 0.5,
                dir,
                normal * sign,
                length,
                tree_assets,
                curb,
            );
            // R3.11 step 11.5: utility poles, street lamps and the wires
            // between them, on this side's curb line. Its RNG salt is
            // private to the furniture, so nothing upstream moves. Village
            // curbs double the pole line (2026-11: dense, messy wires over
            // the lanes); the flag must match the overhead replay's.
            crate::world::props::place_street_furniture(
                &mut plan,
                seed,
                edge_index as u32,
                side,
                (a + b) * 0.5,
                dir,
                normal * sign,
                length,
                curb,
                block.kind == BlockType::UrbanVillage,
            );
        }
        // R3.13: the cross-street overhead layer -- cables, long lamp brackets
        // and scaffolds, everything that hangs OVER the road instead of
        // standing beside it. Placed once per edge rather than per side,
        // because a span belongs to both curbs at once. It replays the pole
        // and tree streams instead of drawing from them, so no approved
        // position moves.
        crate::world::overhead::place_overhead_furniture(
            &mut plan,
            graph,
            seed,
            edge_index,
            tree_assets,
        );
    }

    for (block_index, block) in graph.blocks.iter().enumerate() {
        let anchor = block.center();
        if anchor_chunk(anchor) != chunk {
            continue;
        }
        let mut rng = edge_rng(seed, block_index as u32, 5000);
        match block.kind {
            BlockType::Forest => {
                if tree_assets > 0 {
                    place_forest_trees(&mut plan, &mut rng, block, tree_assets);
                } else {
                    plan.trees_ready = false;
                }
            }
            BlockType::Plaza => {
                plan.objects.push(PlazaPadObject::plan(block.center()));
                if tree_assets > 0 {
                    place_plaza_trees(&mut plan, &mut rng, block, tree_assets);
                } else {
                    plan.trees_ready = false;
                }
            }
            _ => {}
        }
        // ADR-0008: a subdivided block is furnished plot by plot. Each plot
        // is one lot; its OPEN sides (a lane edge, or a stretch of the block
        // outline that faces a street) each get one row of houses, and a lot
        // open on two opposite sides splits its depth between them. That is
        // the same "one row per open face" rule the whole-block pass uses,
        // applied to the smaller polygon — so a lane and a street both end up
        // lined, and the lot's middle is filled by the two rows meeting.
        if !block.children.is_empty()
            && let Some(params) = BuildingParams::of(block.kind)
        {
            for (child_index, child) in block.children.iter().enumerate() {
                // One stack per open face, each confined to HALF the lot
                // depth. Two faces then meet at the midpoint without
                // overlapping; one face fills the whole lot. The band loop
                // respects `share`, so a two-faced lot gets two half stacks
                // instead of one stack that runs through the other (measured
                // earlier at 111-146% coverage, houses inside houses).
                let faces = open_faces(block, child);
                let face_count = faces.len();
                for (face_index, (a, b)) in faces.into_iter().enumerate() {
                    let face_len = a.distance(b);
                    if face_len < MIN_FACADE {
                        continue;
                    }
                    let dir = (b - a).normalize_or_zero();
                    if dir == bevy::math::Vec2::ZERO {
                        continue;
                    }
                    // Inward normal: from the open side into the plot.
                    let plot_center = crate::world::geometry::polygon_centroid(&child.polygon);
                    let mid = (a + b) * 0.5;
                    let mut inward = bevy::math::Vec2::new(-dir.y, dir.x);
                    if inward.dot(plot_center - mid) < 0.0 {
                        inward = -inward;
                    }
                    // A face that lies on the BLOCK OUTLINE is a street side,
                    // not a lane: the plot boundary there stands on the
                    // block's own inset, which is narrower than the street's
                    // shared half width. Push the row inward by that
                    // difference so it clears the street corridor the
                    // project's invariant measures.
                    let on_street = face_on_block_outline(block, a, b);
                    let curb = if on_street {
                        let shared = street_half_width_at(graph, block, mid, dir);
                        // Clear the street corridor the project measures,
                        // plus the row's own clearance.
                        (shared - block.street_width * 0.5).max(0.0) + ROW_CLEARANCE
                    } else {
                        0.0
                    };
                    let mut rng = edge_rng(
                        seed,
                        block_index as u32,
                        14_000 + child_index as u32 * 16 + face_index as u32,
                    );
                    // Two full-depth rows need room for both, setbacks
                    // included. A lot that cannot hold them takes ONE row at
                    // full depth: halving the budget anyway squeezed each row
                    // to half the table depth and left a bare courtyard
                    // between them (the hollow apartment block on the map).
                    let face_depth = lot_extent_along(&child.polygon, inward);
                    let two_rows_fit =
                        face_count >= 2 && face_depth >= 2.0 * (params.setback + params.depth.1);
                    let share = if two_rows_fit { 0.5 } else { 1.0 };
                    place_buildings(
                        graph,
                        &mut plan,
                        &mut rng,
                        block,
                        &child.polygon,
                        &params,
                        mid,
                        dir,
                        inward,
                        face_len,
                        curb,
                        share,
                    );
                }
            }
        }
    }

    // The scenery band behind the ring road (`world::band`): the buildings and
    // trees the walker sees across the ring and can never reach. It is planned
    // from pure geometry rather than from the graph -- the band owns no block,
    // no edge and no street, only the view.
    crate::world::band::plan(&mut plan, seed, chunk, tree_assets);

    plan
}

/// The plaza's 10 x 10 m paved pad (original `_place_plaza_objects`).
struct PlazaPadObject;

impl PlazaPadObject {
    fn plan(center: bevy::math::Vec2) -> PlacedObject {
        PlacedObject {
            kind: PlacedKind::PlazaPad,
            position: center,
            yaw: 0.0,
            size: bevy::math::Vec3::new(10.0, 0.2, 10.0),
            footprint_half: bevy::math::Vec2::splat(5.0),
            wear: 0.0,
            variant: 0,
            tree: None,
            base: 0.0,
        }
    }
}

/// Places the buildings of one block along one street edge, bounded by
/// `polygon` — the block's own outline, or one sub-block plot when the
/// block is subdivided (ADR-0008).
///
/// The row runs along the frontage; `curb` is the distance from the row's
/// reference line (a street centerline, or a lane edge for a plot) to the
/// frontage. `depth_share` is how much of the lot's depth this row may use:
/// 1.0 for a whole-block street row, and for a plot the share implied by how
/// many rows that lot receives.
#[allow(clippy::too_many_arguments)]
fn place_buildings(
    graph: &LayoutGraph,
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    block: &crate::world::layout_graph::LayoutBlock,
    polygon: &[bevy::math::Vec2],
    params: &BuildingParams,
    edge_mid: bevy::math::Vec2,
    edge_dir: bevy::math::Vec2,
    edge_normal: bevy::math::Vec2,
    edge_length: f32,
    curb: f32,
    depth_share: f32,
) {
    let (length_min, length_max) = params.length;
    let (depth_min, depth_max) = params.depth;
    let (height_min, height_max) = params.height;
    let facade_length: f32 = rng.random_range(length_min..=length_max);
    let rand_depth: f32 = rng.random_range(depth_min..=depth_max);
    let height: f32 = rng.random_range(height_min..=height_max);

    // Yaw that puts the building's LENGTH along the street.
    //
    // `rotated_corners` (and Bevy's `Quat::from_rotation_y`) maps local +X to
    // `(cos y, -sin y)` in the ground plane, so aligning +X with `edge_dir`
    // needs `cos y = dir.x, -sin y = dir.z` -- hence the negated z. The
    // previous `atan2(dir.x, dir.z)` put local +Z along the street instead,
    // and every table-sized footprint failed the corridor test.
    let angle = (-edge_dir.y).atan2(edge_dir.x);

    // Facades are metric (R3.11): the wall texture repeats every 4 bays and
    // its UVs come from the building's own size, so a frontage that is not a
    // whole number of bays ends with a sliced window at the corner.
    let dims = FacadeStyle::of(block.kind).map(|style| style.dims());
    let facade_length = match dims {
        Some(d) => snap_to_bay(facade_length, d.bay),
        None => facade_length,
    };

    let is_plot = !std::ptr::eq(polygon, block.polygon.as_slice());

    // A lot row must stop a hair short of the lot's own side edges: the
    // containment test rejects a footprint whose corner sits exactly on the
    // boundary (it cannot tell "flush" from "crossing"), so a fully tiled row
    // would reject itself. 8 cm is invisible at walking scale and far below
    // the 0.25 m drainage setback the village walls actually keep.
    let row_clearance = if is_plot { ROW_CLEARANCE } else { 0.0 };

    // The span of the street edge this row can use. `u_lo..u_hi` is the
    // frontage actually backed by the block/plot, in fractions of the edge.
    let (start, end) = edge_span(edge_mid, edge_dir, edge_length);
    let (u_lo, u_hi) = usable_frontage(polygon, start, end);
    // A plot is a tilted quad: its own extent along the frontage direction is
    // what the row may span, and it can be shorter than the street edge's
    // projection through it. Measuring the polygon directly keeps the row
    // inside the lot instead of overhanging the neighbour lane.
    let frontage = if is_plot {
        polygon_frontage(polygon, edge_dir).min((u_hi - u_lo) * edge_length)
    } else {
        (u_hi - u_lo) * edge_length
    };
    if frontage < MIN_FACADE {
        return;
    }
    let row_frontage = frontage - 2.0 * row_clearance;
    if row_frontage < MIN_FACADE {
        return;
    }

    // House length along the frontage.
    //
    // ADR-0008 rule 4: a PLOT is one lot, so its houses TILE the frontage —
    // flush against each other, no gap at either end. An 11 m village plot
    // takes two 5.5 m houses, not one 6 m house with dirt either side.
    // Whole-block street rows keep the old table-then-modules behaviour.
    let (count, building_length) = if is_plot {
        let target = (params.length.0 + params.length.1) * 0.5;
        tiled_row(row_frontage, target, dims.map(|d| d.bay), params.spacing)
    } else {
        let mut building_length = facade_length.max(MIN_FACADE);
        if frontage < building_length {
            let (lmin, lmax) = if params.length.1 - params.length.0 > 4.0 {
                (MIN_FACADE, MODULE_LENGTH_MAX)
            } else {
                params.length
            };
            building_length = (lmin + (lmax - lmin) * 0.5).min(frontage);
            if let Some(d) = dims {
                building_length = snap_to_bay(building_length, d.bay);
            }
            if frontage < building_length {
                return;
            }
        }
        (
            columns_for(frontage, building_length, params.spacing).len(),
            building_length,
        )
    };
    if building_length < MIN_FACADE || count == 0 {
        return;
    }

    // Column anchors (fractions along the frontage) and the row's depth
    // budget. A perturbed block is a quadrilateral, so its chord varies by
    // metres along the frontage; clamping every column to the shallowest one
    // used to reject the whole row whenever the last column came up shallow.
    let step = (u_hi - u_lo) / count as f32;
    let columns: Vec<f32> = (0..count).map(|i| u_lo + (i as f32 + 0.5) * step).collect();
    // Per-column depth budget. A perturbed lot is a quad (often 5-6 sided
    // once the block perimeter is clipped), so its depth varies by metres
    // along the frontage. Using the SHALLOWEST column for the whole row made
    // an L-shaped lot build only to its narrowest point, leaving the deep
    // half bare; each column now carries its own budget.
    let budgets: Vec<f32> = columns
        .iter()
        .map(|t| {
            depth_along_polygon(polygon, start + (end - start) * *t, edge_normal) * depth_share
        })
        .collect();
    let budget = budgets.iter().copied().fold(0.0_f32, f32::max);
    let min_depth = if is_plot { 2.0 } else { MIN_FACADE };
    if budget - params.setback < min_depth {
        return;
    }

    // Bands. A street row is one band deep. A PLOT is a lot, and ADR-0008
    // fills it with back-to-back rows ("房排") from the frontage inward: a
    // 7-8 m village lot takes one, a ~20 m old-city lot two or three. Bands
    // stop when the lot runs out, so the last one closes against the back
    // edge instead of leaving a bare strip.
    // ADR-0008 step 3: a lot is filled by back-to-back rows ("房排") from its
    // frontage inward. Each band is fitted to the lot's own chord at that
    // depth (`fit_length_at`), so a tilted lot still gets a clean stack
    // instead of one row and a bare strip behind it.
    let max_bands = if is_plot { 8 } else { 1 };
    let mut offset = params.setback;
    let mut band = 0;
    while band < max_bands {
        let remaining = budget - offset;
        if remaining < min_depth {
            break;
        }
        let band_depth = rand_depth.max(min_depth).min(remaining);

        for (column_index, t) in columns.iter().enumerate() {
            let along = (*t - 0.5) * edge_length;
            // This column's own room in the band, so a column near a clipped
            // corner still gets a (shallower) house instead of being dropped.
            let column_remaining = budgets[column_index] - offset;
            if column_remaining < min_depth {
                continue;
            }
            let band_depth = band_depth.min(column_remaining);
            // A plot is a tilted quad whose width shrinks with depth. A row
            // cut to one length for the whole frontage overhangs the lot at
            // its narrow end and gets rejected column by column, leaving the
            // plot empty. Each column takes the longest bay-aligned length
            // that still fits the lot AT ITS OWN position.
            let column_perp = curb + offset + band_depth * 0.5;
            let column_length = if is_plot {
                fit_length_at(
                    polygon,
                    start,
                    end,
                    *t,
                    column_perp,
                    edge_normal,
                    building_length,
                    dims.map(|d| d.bay),
                )
            } else {
                building_length
            };
            if column_length < MIN_FACADE {
                continue;
            }
            // Depth also adapts: a tilted lot narrows with depth, so the
            // band that fits at the frontage may not fit further in. Try the
            // band depth, then progressively shallower houses, so a lot gets
            // built up to its back edge instead of staying empty.
            let mut placed_here = false;
            for attempt in 0..4 {
                let depth_try = match attempt {
                    0 => band_depth,
                    1 => band_depth * 0.75,
                    2 => band_depth * 0.5,
                    _ => band_depth.min(min_depth.max(2.0)),
                };
                if depth_try < min_depth {
                    continue;
                }
                let perp_try = curb + offset + depth_try * 0.5;
                let position = edge_mid + edge_dir * along + edge_normal * perp_try;
                let half = bevy::math::Vec2::new(column_length, depth_try) * 0.5;
                if !rect_clear_of_roads(graph, position, angle, column_length, depth_try) {
                    continue;
                }
                if !oriented_rect_inside_polygon(position, half, angle, polygon) {
                    continue;
                }
                if !oriented_rect_inside_polygon(position, half, angle, &convex_hull(polygon)) {
                    continue;
                }
                if !within_bounds(graph, position, column_length.max(depth_try) * 0.5) {
                    continue;
                }
                let height = match dims {
                    Some(d) => {
                        let floors = (height / d.floor).round().max(1.0);
                        floors * d.floor
                    }
                    None => height,
                };
                let object = PlacedObject {
                    kind: PlacedKind::Building(block.kind),
                    position,
                    yaw: angle,
                    size: bevy::math::Vec3::new(column_length, height, depth_try),
                    footprint_half: half,
                    wear: rng.random_range(0.0..=1.0),
                    variant: facade_variant(position),
                    tree: None,
                    base: 0.0,
                };
                if let Some(style) = FacadeStyle::of(block.kind) {
                    crate::world::props::place_building_props(plan, &object, style, -edge_normal);
                }
                plan.objects.push(object);
                placed_here = true;
                break;
            }
            if placed_here {
                continue;
            }
            let perp = curb + offset + band_depth * 0.5;
            let half = bevy::math::Vec2::new(column_length, band_depth) * 0.5;
            let position = edge_mid + edge_dir * along + edge_normal * perp;
            // Sub-block rows skip the corridor filter: the plot lies inside
            // its block, and the block polygon IS the street's negative
            // shape, so containment already keeps the house off every
            // street. The filter measures against the SHARED edge width (the
            // average of two blocks), which is wider than a village block's
            // own inset whenever it faces a wide neighbour — it rejected
            // perfectly legal lane rows.
            // The corridor filter measures against the SHARED edge width
            // (the average of the two flanking blocks), which is wider than a
            // village block's own inset whenever it faces a wide neighbour.
            // A PLOT row is therefore tested against its OWN block's half
            // width: the street's real curb on that side, not a neighbour's.
            // The plot's own lane edge IS its frontage line: a house stands
            // `setback` behind it, so testing against lane corridors would
            // reject every legal lane row. Only tier-0 streets matter here,
            // and only on the plot's own block side.
            // Whole-block rows use the shared-width corridor test. A PLOT
            // row sits on its block's own inset, which can be NARROWER than
            // the shared half width (a 2.4 m village inset on a street shared
            // with a 9 m old-city block). The project's invariant — asserted
            // by `buildings_stay_out_of_streets_and_inside_their_block` — is
            // measured from the centerline with the shared half width, so a
            // plot row must clear that too, on tier-0 streets only (its own
            // lane edge IS its frontage, so lanes are excluded).
            // Plot rows obey the same corridor rule as street rows: the
            // project's invariant measures the whole city against the SHARED
            // street half width, so a plot row that cannot clear it must be
            // pushed in (or dropped), exactly like a whole-block row.
            let corridor_hit =
                !rect_clear_of_roads(graph, position, angle, column_length, band_depth);
            if corridor_hit {
                continue;
            }
            if !oriented_rect_inside_polygon(position, half, angle, polygon) {
                continue;
            }
            if !oriented_rect_inside_polygon(position, half, angle, &convex_hull(polygon)) {
                continue;
            }
            if !within_bounds(graph, position, column_length.max(band_depth) * 0.5) {
                continue;
            }

            // Height snaps to whole floors for the same reason as the
            // length: the shader's `v` counts floors from the roofline.
            // Rounding, NOT ceiling: `ceil` on a 3-6 m range with a 3 m
            // storey turned every old-city building into exactly 6 m.
            let height = match dims {
                Some(d) => {
                    let floors = (height / d.floor).round().max(1.0);
                    floors * d.floor
                }
                None => height,
            };

            let object = PlacedObject {
                kind: PlacedKind::Building(block.kind),
                position,
                yaw: angle,
                // MUST be the length that was tested (`column_length`), not
                // the nominal row length: a plot column is shortened to fit
                // its lot, and publishing the nominal length drew a house
                // wider than the corridor test had approved.
                size: bevy::math::Vec3::new(column_length, height, band_depth),
                footprint_half: half,
                wear: rng.random_range(0.0..=1.0),
                variant: facade_variant(position),
                tree: None,
                base: 0.0,
            };
            if let Some(style) = FacadeStyle::of(block.kind) {
                crate::world::props::place_building_props(plan, &object, style, -edge_normal);
            }
            plan.objects.push(object);
        }

        band += 1;
        offset += band_depth + ROW_CLEARANCE;
    }
}

/// Facade pattern variant for a building at `position` (R3.11).
///
/// Hashed from the position rather than drawn from the RNG on purpose: the
/// per-edge stream already produces the wear grade, and taking one more draw
/// would shift every downstream value, changing the weathering of the whole
/// city for a purely cosmetic addition. Position hashing is also what makes
/// neighbouring buildings differ: two buildings on the same edge sit at
/// different points, so they almost never land on the same variant.
pub fn facade_variant(position: bevy::math::Vec2) -> u8 {
    // 0.37 m of quantisation, so tiny float differences within one building's
    // own footprint cannot flip the variant mid-bake.
    let x = (position.x * 2.7).floor() as i64;
    let z = (position.y * 2.7).floor() as i64;
    let mixed = (x.wrapping_mul(0x9E37_79B1) ^ z.wrapping_mul(0x85EB_CA77)) as u64;
    (mixed >> 13) as u8 % VARIANT_COUNT
}

/// Extra margin the planner keeps beyond a street's half width (m), so a
/// building that passes planning never trips the invariant's strict test.
const CORRIDOR_SAFETY: f32 = 0.02;

/// Invisible margin kept between a lot row and its lot's own edges (m).
/// The containment test rejects a footprint whose edge lies exactly on the
/// lot boundary (it cannot tell "flush" from "crossing"), so a fully built
/// lot needs this hair of slack; at walking scale it is invisible and far
/// below the 0.25 m drainage setback the walls already keep.
const ROW_CLEARANCE: f32 = 0.08;

/// Smallest facade length/depth tried before giving up on a block frontage
/// (m). A single 50 m cell leaves ~39 m inside its inset outline, so even the
/// largest type has to come down to this before a 1x1 block can hold a row.
const MIN_FACADE: f32 = 3.0;

/// Middle-of-the-road module size used when a block cannot hold the table
/// footprint (m): the old city's rhythm, applied to any type.
const MODULE_LENGTH_MAX: f32 = 6.0;

/// Material wear buckets (R3.6). Buildings pick a bucket from their planned
/// `wear` level; each bucket owns one shared `StandardMaterial` per block
/// type, so the whole city costs `types × buckets` materials and the meshes
/// keep batching inside a bucket.
pub(crate) const WEAR_BUCKETS: usize = 4;

/// Bucket index for a planned wear level (0 = fresh .. 3 = heavily worn).
pub(crate) fn wear_bucket(wear: f32) -> usize {
    ((wear.clamp(0.0, 1.0) * WEAR_BUCKETS as f32) as usize).min(WEAR_BUCKETS - 1)
}

/// Column anchors (fraction along the edge) for a row of `count` buildings.
fn build_columns(count: usize) -> Vec<f32> {
    (0..count)
        .map(|i| (i as f32 + 0.5) / count.max(1) as f32)
        .collect()
}

/// Column anchors for a row of `length`-long buildings along an edge.
fn columns_for(edge_length: f32, length: f32, spacing: f32) -> Vec<f32> {
    let count = ((edge_length + spacing) / (length + spacing))
        .floor()
        .max(1.0) as usize;
    build_columns(count)
}

/// Tiles one lot's frontage with houses: `(count, house_length)`.
///
/// ADR-0008 rule 4: the plots inside a row are flush, and so are the houses
/// inside a plot — a lot is fully built up, never a house with a gap either
/// side. The count is the nearest whole number of houses that averages
/// `target` metres of frontage, clamped to what `min_len` allows, and the
/// returned length is the exact tile width (frontage / count) so the last
/// house closes the lot. `bay` snaps the count to whole facade bays when the
/// tile is still wide enough, keeping every window the same physical size.
fn tiled_row(frontage: f32, target: f32, bay: Option<f32>, spacing: f32) -> (usize, f32) {
    let pitch = (target + spacing).max(0.5);
    let raw = ((frontage + spacing) / pitch).round().max(1.0) as usize;
    match bay {
        // Whole BAYS per house, and the row is then measured as whole bays
        // too: the facade shader derives `u` from the building's own size, so
        // a 6.07 m frontage on a 2 m bay cuts the last window in half. Taking
        // the largest whole-bay length that still fits the lot keeps every
        // window physically identical, and the tiny leftover is absorbed by
        // the row's own clearance.
        Some(bay) if bay > 0.0 => {
            let bays_total = (frontage / bay).floor().max(1.0) as usize;
            let bays_per_house = (bays_total as f32 / raw as f32).round().max(1.0) as usize;
            let count = (bays_total / bays_per_house).max(1);
            // The returned length is ONE house, not the whole row: returning
            // the row total placed a single 42 m bar across a 48 m lot instead
            // of seven 6 m houses.
            (count, bays_per_house as f32 * bay)
        }
        _ => {
            let length = (frontage - spacing * (raw as f32 - 1.0)).max(0.5) / raw as f32;
            (raw, length)
        }
    }
}

/// Corridor test with an explicit tolerance override (m). Used by plot rows,
/// which must clear tier-0 corridors measured the same way the project's
/// invariant does, while their own lane face is exempt.
fn rect_clear_of_roads(
    graph: &LayoutGraph,
    center: bevy::math::Vec2,
    yaw: f32,
    length: f32,
    depth: f32,
) -> bool {
    let corners = rotated_corners(center, yaw, length, depth);
    // EVERY edge is checked, lanes included: the invariant that guards the
    // city walks the whole edge list without distinguishing tiers, and a lane
    // is negative space a building must not occupy either. A plot row still
    // clears its own lane because it stands `setback` behind the lane edge.
    for edge in &graph.edges {
        let a = graph.nodes[edge.from as usize].pos;
        let b = graph.nodes[edge.to as usize].pos;
        // 2 cm of extra margin: the invariant's own threshold is
        // `width/2 - 1e-3`, and a footprint that lands exactly on it can tip
        // either way after the transform round-trip.
        let half = edge.width * 0.5 + CORRIDOR_SAFETY;
        if corners
            .iter()
            .any(|c| dist_point_to_segment(*c, a, b) < half)
        {
            return false;
        }
        for i in 0..4 {
            let (p, q) = (corners[i], corners[(i + 1) % 4]);
            if dist_segment_to_segment(p, q, a, b) < half {
                return false;
            }
        }
    }
    true
}

/// True when a plot face lies on the parent block's outline (a street side).
fn face_on_block_outline(
    block: &crate::world::layout_graph::LayoutBlock,
    a: bevy::math::Vec2,
    b: bevy::math::Vec2,
) -> bool {
    let mid = (a + b) * 0.5;
    let m = block.polygon.len();
    m >= 3
        && (0..m).any(|j| {
            crate::world::geometry::dist_point_to_segment(
                mid,
                block.polygon[j],
                block.polygon[(j + 1) % m],
            ) < 0.05
        })
}

/// Shared half width of the tier-0 road a block face looks at. The arterial
/// counts as a road (ADR-0009): a block facing the cross retreats by its
/// half width like any other street.
fn street_half_width_at(
    graph: &LayoutGraph,
    block: &crate::world::layout_graph::LayoutBlock,
    mid: bevy::math::Vec2,
    _dir: bevy::math::Vec2,
) -> f32 {
    let mut best = block.street_width * 0.5;
    let mut found = false;
    for edge in &graph.edges {
        if edge.kind == crate::world::layout_graph::EdgeKind::Lane {
            continue;
        }
        if edge.left_block != Some(block.id) && edge.right_block != Some(block.id) {
            continue;
        }
        let a = graph.nodes[edge.from as usize].pos;
        let b = graph.nodes[edge.to as usize].pos;
        if crate::world::geometry::dist_point_to_segment(mid, a, b) > 12.0 {
            continue;
        }
        if !found || edge.width * 0.5 > best {
            best = edge.width * 0.5;
            found = true;
        }
    }
    best
}

/// The open sides of a plot, as `(a, b)` segments in plot order.
///
/// An "open side" is either a lane frontage or a stretch of the plot's
/// boundary that lies on the block outline (that is where the plot meets a
/// tier-0 street). Sides shared with a NEIGHBOUR plot are not open: they take
/// the party wall of whatever faces them.
fn open_faces(
    block: &crate::world::layout_graph::LayoutBlock,
    child: &crate::world::layout_graph::SubBlock,
) -> Vec<(bevy::math::Vec2, bevy::math::Vec2)> {
    let n = child.polygon.len();
    let m = block.polygon.len();
    if n < 3 || m < 3 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..n {
        let a = child.polygon[i];
        let b = child.polygon[(i + 1) % n];
        if a.distance(b) < MIN_FACADE {
            continue;
        }
        let mid = (a + b) * 0.5;
        // Lane side?
        let is_lane = child
            .frontages
            .iter()
            .any(|f| crate::world::geometry::dist_point_to_segment(mid, f.a, f.b) < 0.05);
        // Street side? (lies on the parent block's outline)
        let is_street = (0..m).any(|j| {
            crate::world::geometry::dist_point_to_segment(
                mid,
                block.polygon[j],
                block.polygon[(j + 1) % m],
            ) < 0.05
        });
        if is_lane || is_street {
            out.push((a, b));
        }
    }
    // A plot cut by the lane-less fallback grid has neither a lane frontage
    // nor reachable street side, yet it must still be built up. Fall back to
    // its own longest edge as the building line: the row then faces the
    // neighbouring plot across the strip, which is exactly the row-and-gap
    // rhythm the fallback grid was cut for.
    if out.is_empty() {
        let mut best = (0usize, 0.0_f32);
        for i in 0..n {
            let len = child.polygon[i].distance(child.polygon[(i + 1) % n]);
            if len > best.1 {
                best = (i, len);
            }
        }
        if best.1 >= MIN_FACADE {
            out.push((child.polygon[best.0], child.polygon[(best.0 + 1) % n]));
        }
    }
    out
}

/// Longest bay-aligned house length that still fits the lot at column `t`.
///
/// The row's centreline sits at fraction `t` along the frontage and `perp`
/// metres inside it. The free run available there is the lot's chord along
/// the frontage direction at that point; the returned length is that run
/// minus the lot's side clearance, snapped DOWN to whole facade bays (so no
/// window is sliced) and capped at the row's nominal length.
#[allow(clippy::too_many_arguments)]
fn fit_length_at(
    polygon: &[bevy::math::Vec2],
    a: bevy::math::Vec2,
    b: bevy::math::Vec2,
    t: f32,
    perp: f32,
    normal: bevy::math::Vec2,
    nominal: f32,
    bay: Option<f32>,
) -> f32 {
    let point = a + (b - a) * t + normal * perp;
    let dir = (b - a).normalize_or_zero();
    if dir == bevy::math::Vec2::ZERO {
        return 0.0;
    }
    // Extent along the FRONTAGE direction at this point: `depth_along_polygon`
    // returns the span of the polygon along the direction it is given, so
    // passing `dir` measures the row length available here. Passing the
    // normal by mistake measured the DEPTH and sized every house to the lot's
    // depth instead of its width.
    let chord = depth_along_polygon(polygon, point, dir);
    let usable = (chord - 2.0 * ROW_CLEARANCE).min(nominal).max(0.0);
    match bay {
        Some(bay) if bay > 0.0 => {
            let bays = (usable / bay).floor().max(0.0);
            bays * bay
        }
        _ => usable,
    }
}

/// Extent of a polygon along `dir` (m): how deep the lot is in that
/// direction.
fn lot_extent_along(polygon: &[bevy::math::Vec2], dir: bevy::math::Vec2) -> f32 {
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in polygon {
        let d = dir.dot(*p);
        lo = lo.min(d);
        hi = hi.max(d);
    }
    (hi - lo).max(0.0)
}

/// Extent of a polygon along `dir` (m) — the length a row may span.
fn polygon_frontage(polygon: &[bevy::math::Vec2], dir: bevy::math::Vec2) -> f32 {
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in polygon {
        let d = dir.dot(*p);
        lo = lo.min(d);
        hi = hi.max(d);
    }
    (hi - lo).max(0.0)
}

/// Start and end points of the edge a row is laid along, from its midpoint,
/// direction and length (the plan only keeps the midpoint around).
fn edge_span(
    mid: bevy::math::Vec2,
    dir: bevy::math::Vec2,
    length: f32,
) -> (bevy::math::Vec2, bevy::math::Vec2) {
    (mid - dir * (length * 0.5), mid + dir * (length * 0.5))
}

/// Places the street trees of one block along one street edge (inside the
/// setback strip, one candidate every [`TREE_SPACING`] metres).
#[allow(clippy::too_many_arguments)]
fn place_street_trees(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    params: &BuildingParams,
    edge_mid: bevy::math::Vec2,
    edge_dir: bevy::math::Vec2,
    edge_normal: bevy::math::Vec2,
    edge_length: f32,
    tree_assets: u8,
    curb: f32,
) {
    if params.street_tree_probability <= 0.0 || tree_assets == 0 {
        return;
    }
    // Half the setback: the tree stands between curb and facade.
    let perp = curb + params.setback * 0.5;
    let line = street_tree_line(
        rng,
        params.street_tree_probability,
        edge_length,
        tree_assets,
    );
    for (along, tree) in line {
        let position = edge_mid + edge_dir * along + edge_normal * perp;
        plan.objects.push(PlacedObject {
            kind: PlacedKind::StreetTree,
            position,
            yaw: tree.yaw,
            size: bevy::math::Vec3::ZERO,
            footprint_half: bevy::math::Vec2::ZERO,
            wear: 0.0,
            variant: 0,
            tree: Some(tree),
            base: 0.0,
        });
    }
}

/// The street trees one edge side actually grows, as `(along offset,
/// instance)` pairs.
///
/// Single source of truth for the tree stream's draw order: one probability
/// roll per candidate (drawn whether or not it is used, so the stream stays
/// stable) and three instance draws per tree. Both the plan and the
/// cross-street pass in `world::overhead` go through it, so the overhead layer
/// sees the real trunks instead of a second guess at where they are.
fn street_tree_line(
    rng: &mut StdRng,
    probability: f32,
    edge_length: f32,
    tree_assets: u8,
) -> Vec<(f32, TreeInstance)> {
    let candidates = (edge_length / TREE_SPACING).floor().max(1.0) as usize;
    let mut line = Vec::new();
    for i in 0..candidates {
        let roll: f32 = rng.random();
        if roll > probability {
            continue;
        }
        let t = (i as f32 + 0.5) / candidates as f32;
        let along = (t - 0.5) * edge_length;
        line.push((
            along,
            TreeInstance {
                asset: rng.random_range(0..tree_assets),
                scale: rng.random_range(TREE_SCALE_MIN..=TREE_SCALE_MAX),
                yaw: rng.random_range(0.0..std::f32::consts::TAU),
            },
        ));
    }
    line
}

/// Along-offsets of the street trees one edge side grows, without building a
/// plan.
///
/// A replay of [`street_tree_line`] on the same per-edge salt stream, for
/// placement rules that must not grow through a tree. The along offset is
/// fixed before the three instance draws, so any positive `tree_assets` yields
/// the same offsets -- callers that only need the positions can pass `1`.
pub fn street_tree_alongs(
    seed: u64,
    edge_index: u32,
    side: u32,
    probability: f32,
    edge_length: f32,
    tree_assets: u8,
) -> Vec<f32> {
    if probability <= 0.0 || tree_assets == 0 {
        return Vec::new();
    }
    let mut rng = edge_rng(seed, edge_index, 1000 + side * 16);
    street_tree_line(&mut rng, probability, edge_length, tree_assets)
        .into_iter()
        .map(|(along, _)| along)
        .collect()
}

/// Dense trees filling a FOREST block (the original `_place_trees_in_forest_block`).
fn place_forest_trees(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    block: &crate::world::layout_graph::LayoutBlock,
    tree_assets: u8,
) {
    let (min, max) = polygon_bounds(&block.polygon);
    let size = max - min;
    let count = ((size.x * size.y) / (TREE_SPACING * TREE_SPACING))
        .floor()
        .clamp(1.0, 6.0) as usize;
    let center = (min + max) * 0.5;

    for _ in 0..count {
        let offset = bevy::math::Vec2::new(
            rng.random_range(-size.x * 0.35..=size.x * 0.35),
            rng.random_range(-size.y * 0.35..=size.y * 0.35),
        );
        let position = center + offset;
        if !point_in_polygon(position, &block.polygon) {
            continue;
        }
        push_tree(plan, rng, PlacedKind::ForestTree, position, tree_assets);
    }
}

/// A couple of trees on a plaza, inside its polygon.
fn place_plaza_trees(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    block: &crate::world::layout_graph::LayoutBlock,
    tree_assets: u8,
) {
    let (min, max) = polygon_bounds(&block.polygon);
    let size = max - min;
    let center = (min + max) * 0.5;
    let count = ((size.x * size.y) / 100.0).floor().clamp(1.0, 2.0) as usize;
    for _ in 0..count {
        let offset = bevy::math::Vec2::new(
            rng.random_range(-size.x * 0.25..=size.x * 0.25),
            rng.random_range(-size.y * 0.25..=size.y * 0.25),
        );
        let position = center + offset;
        if !point_in_polygon(position, &block.polygon) {
            continue;
        }
        push_tree(plan, rng, PlacedKind::PlazaTree, position, tree_assets);
    }
}

/// Appends one tree instance: random asset, scale and yaw (original
/// `_instantiate_tree`).
fn push_tree(
    plan: &mut ChunkPlan,
    rng: &mut StdRng,
    kind: PlacedKind,
    position: bevy::math::Vec2,
    tree_assets: u8,
) {
    let asset: u8 = rng.random_range(0..tree_assets);
    let scale: f32 = rng.random_range(TREE_SCALE_MIN..=TREE_SCALE_MAX);
    let yaw: f32 = rng.random_range(0.0..std::f32::consts::TAU);
    plan.objects.push(PlacedObject {
        kind,
        position,
        yaw,
        size: bevy::math::Vec3::ZERO,
        footprint_half: bevy::math::Vec2::ZERO,
        wear: 0.0,
        variant: 0,
        tree: Some(TreeInstance { asset, scale, yaw }),
        base: 0.0,
    });
}

/// Bounding box of a polygon as `(min, max)`.
fn polygon_bounds(polygon: &[bevy::math::Vec2]) -> (bevy::math::Vec2, bevy::math::Vec2) {
    let mut min = bevy::math::Vec2::splat(f32::INFINITY);
    let mut max = bevy::math::Vec2::splat(f32::NEG_INFINITY);
    for p in polygon {
        min = min.min(*p);
        max = max.max(*p);
    }
    (min, max)
}

/// Depth of `block` measured from an edge point along `outward`.
///
/// Casts a ray from just outside the facade line into the block and returns
/// the distance to the far boundary (m). Flat polygon edges are used rather
/// than the convex hull, because a facade must stay inside the visible
/// outline; the hull check later is the safety net for the collision body.
/// Returns `0` when the ray leaves the polygon immediately.
/// The span of the edge (as fractions of `a..b`) over which the block's own
/// polygon actually runs.
///
/// A street edge spans the full lattice cell side, but the block polygon is
/// inset from the centerline, so its frontage is shorter than the edge by about
/// one street half-width at each end. Projecting the polygon onto the edge
/// measures that frontage instead of guessing it, and it is what lets a row
/// stop before the mitred corners -- where the block's chord collapses to zero
/// and a depth probe would fail the whole row.
///
/// Both fractions are clamped into `0..1`: a polygon that only touches the edge
/// at a point must not produce an inverted span.
fn usable_frontage(
    polygon: &[bevy::math::Vec2],
    a: bevy::math::Vec2,
    b: bevy::math::Vec2,
) -> (f32, f32) {
    let ab = b - a;
    let len2 = ab.length_squared();
    if len2 < 1e-9 {
        return (0.0, 1.0);
    }
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in polygon {
        let t = (*p - a).dot(ab) / len2;
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (lo.clamp(0.0, 1.0), hi.clamp(0.0, 1.0))
}

/// Depth of a block measured from an edge point along `outward` — the
/// public wrapper kept for diagnostics; placement itself probes the
/// polygon it is actually building against (block or plot).
pub fn block_depth_along_pub(
    block: &crate::world::layout_graph::LayoutBlock,
    origin: bevy::math::Vec2,
    normal_out: bevy::math::Vec2,
) -> f32 {
    depth_along_polygon(&block.polygon, origin, normal_out)
}

/// Cross-section depth of `polygon` at `origin` measured along
/// `normal_out` (see [`block_depth_along_pub`] for the ray rationale).
fn depth_along_polygon(
    polygon: &[bevy::math::Vec2],
    origin: bevy::math::Vec2,
    normal_out: bevy::math::Vec2,
) -> f32 {
    // Work in the block's own frame: `t` runs along `normal_out` (towards the
    // street), `u` along the street. Marching from far outside the block
    // towards the street finds the first boundary crossing = the facade, the
    // next one = the back wall. Starting inside (the original code started at
    // the curb, which can already be inside a perturbed polygon) would return
    // a negative "depth" or the near edge instead of the far one.
    let t_out = normal_out.perp_dot(origin);
    let _ = normal_out.dot(origin);
    let n = polygon.len();

    // Intersections of the ray (t varying, u fixed) with the polygon.
    let mut hits: Vec<f32> = Vec::with_capacity(2);
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let (ta, tb) = (normal_out.perp_dot(a), normal_out.perp_dot(b));
        let (ua, ub) = (normal_out.dot(a), normal_out.dot(b));
        if (ta - t_out).abs() < 1e-9 && (tb - t_out).abs() < 1e-9 {
            continue; // Collinear with the ray.
        }
        if (ta - t_out) * (tb - t_out) > 0.0 {
            continue; // Both ends on the same side: no crossing.
        }
        let span = tb - ta;
        if span.abs() < 1e-9 {
            continue;
        }
        let f = (t_out - ta) / span;
        if (0.0..=1.0).contains(&f) {
            hits.push(ua + (ub - ua) * f);
        }
    }
    hits.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // `u` runs along the street axis, `t` across it. The street point is at
    // `u_origin`; the polygon's interval covering `t_out` spans `[near, far]`
    // in `u`. Walking inward from the street means moving away from the street
    // side, so the depth is the span of the interval minus how far the street
    // point already sits inside it (0 when the point is outside the span).
    let mut depth = 0.0;
    for pair in hits.chunks(2) {
        if pair.len() < 2 {
            break;
        }
        let (near, far) = (pair[0], pair[1]);
        // The interval is the block's own cross-section at this `t`; depth is
        // measured from the street side into the block.
        let (lo, hi) = if near < far { (near, far) } else { (far, near) };
        depth = hi - lo;
    }
    depth
}

/// Corners of a footprint rotated by `yaw` around Y, CCW in XZ.
///
/// Yaw follows the renderer: a positive Y rotation maps local `+X` to
/// `(cos, -sin)` and local `+Z` to `(sin, cos)` in world XZ, so the corner
/// offsets below match what the mesh actually covers.
pub fn rotated_corners(
    center: bevy::math::Vec2,
    yaw: f32,
    length: f32,
    depth: f32,
) -> [bevy::math::Vec2; 4] {
    let (s, c) = yaw.sin_cos();
    let (hl, hd) = (length * 0.5, depth * 0.5);
    let offset = |lx: f32, lz: f32| {
        bevy::math::Vec2::new(center.x + lx * c + lz * s, center.y - lx * s + lz * c)
    };
    [
        offset(-hl, -hd),
        offset(hl, -hd),
        offset(hl, hd),
        offset(-hl, hd),
    ]
}

/// True if `position ± extent` stays inside the world square.
fn within_bounds(graph: &LayoutGraph, position: bevy::math::Vec2, extent: f32) -> bool {
    let world = CELL_SIZE * GRID_CELLS as f32;
    let _ = graph;
    position.x - extent >= 0.0
        && position.y - extent >= 0.0
        && position.x + extent <= world
        && position.y + extent <= world
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::geometry::oriented_rect_inside_polygon;
    use crate::world::path_network::generate_world;

    /// Every seed used by the placement assertions.
    const SEEDS: [u64; 3] = [1, 7, crate::world::chunks::DEFAULT_SEED];

    fn all_chunks(_graph: &LayoutGraph) -> Vec<(i32, i32)> {
        let n = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32;
        (0..n).flat_map(|x| (0..n).map(move |z| (x, z))).collect()
    }

    #[test]
    fn parameter_table_matches_adr() {
        let old = BuildingParams::of(BlockType::OldCity).unwrap();
        assert_eq!(old.length, (4.0, 6.0));
        assert_eq!(old.setback, 1.0);
        assert_eq!(old.street_tree_probability, 0.2);
        let apt = BuildingParams::of(BlockType::Apartment).unwrap();
        assert_eq!(apt.length, (25.0, 40.0));
        assert_eq!(apt.street_tree_probability, 0.6);
        let factory = BuildingParams::of(BlockType::Factory).unwrap();
        assert_eq!(factory.length, (50.0, 50.0));
        assert_eq!(factory.setback, 10.0);
        assert_eq!(factory.street_tree_probability, 0.8);
        assert!(BuildingParams::of(BlockType::Plaza).is_none());
        assert!(BuildingParams::of(BlockType::Forest).is_none());
    }

    #[test]
    fn every_chunk_is_planned_deterministically() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            for chunk in [
                (0, 0),
                (7, 7),
                (20, 20),
                (GRAPH_LAST_CHUNK, GRAPH_LAST_CHUNK),
            ] {
                let a = plan_chunk(&graph, seed, chunk, 3);
                let b = plan_chunk(&graph, seed, chunk, 3);
                assert_eq!(a, b, "seed {seed} chunk {chunk:?} must be stable");
            }
        }
    }

    #[test]
    fn every_object_is_placed_exactly_once() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut seen: std::collections::HashMap<(i32, i32, String), usize> =
                std::collections::HashMap::new();
            let mut total = 0;
            for chunk in all_chunks(&graph) {
                let plan = plan_chunk(&graph, seed, chunk, 3);
                for object in plan.objects {
                    total += 1;
                    // Props stack at one xz spot (three wire heights on one
                    // span), so the key carries the mount height too.
                    let key = (
                        chunk.0,
                        chunk.1,
                        format!("{:?}@{:?}@{}", object.kind, object.position, object.base),
                    );
                    *seen.entry(key).or_default() += 1;
                }
            }
            assert!(total > 0, "seed {seed} placed nothing");
            assert!(
                seen.values().all(|count| *count <= 1),
                "seed {seed}: an object was planned into two chunks"
            );
            // Cross-check the total against a single whole-map pass: any
            // object planned twice would inflate the per-chunk sum.
            let distinct = seen.len();
            assert_eq!(total, distinct, "seed {seed}: duplicate placements");
        }
    }

    #[test]
    fn plans_carry_props_next_to_their_buildings() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut props = 0;
            for chunk in all_chunks(&graph) {
                let plan = plan_chunk(&graph, seed, chunk, 3);
                for object in &plan.objects {
                    let PlacedKind::Prop(kind) = object.kind else {
                        continue;
                    };
                    // Street furniture is anchored to the curb/pole line, not
                    // to a building: the along-street run and the R3.13
                    // cross-street layer (drop lines, long brackets,
                    // scaffolds) all hang from `pole_slots` positions.
                    // `streets_carry_poles_at_the_curb` owns that check; here
                    // only building-anchored props are asserted.
                    if matches!(
                        kind,
                        PropKind::Pole
                            | PropKind::LampArm
                            | PropKind::LampHead
                            | PropKind::CrossArm
                            | PropKind::Wire
                            | PropKind::CrossWire
                            | PropKind::LongLampArm
                            | PropKind::LongLampHead
                            | PropKind::ScaffoldPost
                            | PropKind::ScaffoldBeam
                            | PropKind::ScaffoldDeck
                    ) {
                        continue;
                    }
                    props += 1;
                    assert!(
                        plan.objects.iter().any(|b| {
                            matches!(b.kind, PlacedKind::Building(_))
                                && (b.position - object.position).length() <= 28.0
                        }),
                        "seed {seed}: {kind:?} prop at {:?} has no building nearby",
                        object.position
                    );
                }
            }
            assert!(props > 0, "seed {seed}: no props planned at all");
        }
    }

    #[test]
    fn streets_carry_poles_at_the_curb() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut poles = 0;
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    let PlacedKind::Prop(PropKind::Pole) = object.kind else {
                        continue;
                    };
                    poles += 1;
                    // The pole stands `curb + standoff` off its own edge
                    // line; the nearest edge must agree to a hair. ADR-0008:
                    // a lane mouth splits its street into two halves, so a
                    // pole planned for the pre-split midpoint can sit past
                    // a half's endpoint — measure against the street's
                    // infinite LINE (the halves are collinear with it).
                    let miss = graph
                        .edges
                        .iter()
                        .map(|edge| {
                            let a = graph.nodes[edge.from as usize].pos;
                            let b = graph.nodes[edge.to as usize].pos;
                            let ab = b - a;
                            let len2 = ab.length_squared();
                            let dist = if len2 < 1e-9 {
                                object.position.distance(a)
                            } else {
                                let t = (object.position - a).dot(ab) / len2;
                                if t < 0.0 || t > 1.0 {
                                    // Past the segment: distance to the
                                    // line, not to the endpoint.
                                    (object.position - a).perp_dot(ab).abs() / len2.sqrt()
                                } else {
                                    dist_point_to_segment(object.position, a, b)
                                }
                            };
                            (dist - (edge.width * 0.5 + crate::world::props::POLE_STANDOFF)).abs()
                        })
                        .fold(f32::MAX, f32::min);
                    assert!(
                        miss < 0.05,
                        "seed {seed}: pole at {:?} sits {miss} m off every curb line",
                        object.position
                    );
                }
            }
            assert!(poles > 0, "seed {seed}: no utility poles planned");
        }
    }

    #[test]
    fn buildings_stay_out_of_streets_and_inside_their_block() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut checked = 0;
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    let PlacedKind::Building(_) = object.kind else {
                        continue;
                    };
                    checked += 1;
                    let corners =
                        rotated_corners(object.position, object.yaw, object.size.x, object.size.z);
                    for edge in &graph.edges {
                        let a = graph.nodes[edge.from as usize].pos;
                        let b = graph.nodes[edge.to as usize].pos;
                        // Streets keep their own widths (6-10 m): the curb
                        // sits at that street's half-width, not a global 8 m.
                        let half = edge.width * 0.5 - 1e-3;
                        for (i, corner) in corners.iter().enumerate() {
                            assert!(
                                dist_point_to_segment(*corner, a, b) >= half,
                                "seed {seed}: building corner {} at {:?} intrudes into the \
                                 street corridor of edge {}..{}",
                                i,
                                corner,
                                edge.from,
                                edge.to
                            );
                        }
                    }
                    // Same corners the renderer draws: an axis-aligned test
                    // here would validate a box the game never builds.
                    // ADR-0008: a subdivided block's buildings stand inside
                    // one of its PLOTS, so the plots count as containment
                    // too (each plot is inside the parent by construction).
                    assert!(graph.blocks.iter().any(|block| {
                        oriented_rect_inside_polygon(
                            object.position,
                            object.footprint_half,
                            object.yaw,
                            &block.polygon,
                        ) || oriented_rect_inside_polygon(
                            object.position,
                            object.footprint_half,
                            object.yaw,
                            &convex_hull(&block.polygon),
                        ) || block.children.iter().any(|child| {
                            oriented_rect_inside_polygon(
                                object.position,
                                object.footprint_half,
                                object.yaw,
                                &child.polygon,
                            ) || oriented_rect_inside_polygon(
                                object.position,
                                object.footprint_half,
                                object.yaw,
                                &convex_hull(&child.polygon),
                            )
                        })
                    }));
                }
            }
            assert!(checked > 0, "seed {seed}: no buildings placed");
        }
    }

    #[test]
    fn subdivided_blocks_keep_a_building_coverage_floor() {
        // ADR-0008 risk: plots too small -> rows systematically rejected ->
        // empty village blocks. Every subdivided block must still carry
        // buildings covering a meaningful share of its plot area.
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            // Collect every building once, keyed by parent block via plot
            // containment.
            let mut buildings: Vec<(bevy::math::Vec2, f32)> = Vec::new();
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    if matches!(object.kind, PlacedKind::Building(_)) {
                        buildings.push((object.position, object.size.x * object.size.z));
                    }
                }
            }
            let mut checked = 0;
            for block in &graph.blocks {
                if block.children.is_empty() {
                    continue;
                }
                checked += 1;
                let plot_area: f32 = block
                    .children
                    .iter()
                    .map(|c| crate::world::geometry::polygon_area(&c.polygon).abs())
                    .sum();
                let covered: f32 = buildings
                    .iter()
                    .filter(|(pos, _)| {
                        block.children.iter().any(|c| {
                            crate::world::geometry::point_in_convex_inclusive(*pos, &c.polygon)
                        })
                    })
                    .map(|(_, area)| area)
                    .sum();
                let ratio = covered / plot_area.max(1.0);
                // 20% floor: interior plots legitimately carry fewer rows
                // than street-facing ones (a village row is 7-9 m deep on
                // a ~13 m pitch, so roughly half the plot depth is lane +
                // setback by design). The floor catches the failure mode
                // the ADR names — systematic rejection leaving a block
                // EMPTY — not the intended lane share.
                assert!(
                    ratio >= 0.20,
                    "seed {seed}: block {} ({:?}) coverage {:.0}% is below the 20% floor",
                    block.id,
                    block.kind,
                    ratio * 100.0
                );
            }
            assert!(checked > 0, "seed {seed}: no subdivided blocks");
        }
    }

    #[test]
    fn trees_are_skipped_until_assets_load() {
        let (_, graph) = generate_world(SEEDS[0]).unwrap();
        let mut with_assets = 0;
        for chunk in all_chunks(&graph) {
            let plan = plan_chunk(&graph, SEEDS[0], chunk, 3);
            assert!(plan.trees_ready);
            with_assets += plan.tree_count();
            let empty = plan_chunk(&graph, SEEDS[0], chunk, 0);
            assert_eq!(empty.tree_count(), 0, "no assets, no trees");
            assert_eq!(
                empty.building_count(),
                plan.building_count(),
                "buildings do not depend on trees"
            );
            // The plaza pad is not a tree and must survive.
            assert_eq!(
                empty
                    .objects
                    .iter()
                    .filter(|o| o.kind == PlacedKind::PlazaPad)
                    .count(),
                plan.objects
                    .iter()
                    .filter(|o| o.kind == PlacedKind::PlazaPad)
                    .count()
            );
        }
        assert!(with_assets > 0, "the map should carry trees");
    }

    #[test]
    fn tree_probability_ordering_holds() {
        // ADR-0004: old city 20% < apartment 60% < factory 80% per candidate.
        let old = BuildingParams::of(BlockType::OldCity).unwrap();
        let apt = BuildingParams::of(BlockType::Apartment).unwrap();
        let factory = BuildingParams::of(BlockType::Factory).unwrap();
        assert!(old.street_tree_probability < apt.street_tree_probability);
        assert!(apt.street_tree_probability < factory.street_tree_probability);
    }

    #[test]
    fn wear_buckets_cover_the_full_range() {
        // R3.6: every planned wear level maps into a bucket; the extremes
        // hit the first and last bucket and the mapping is monotone.
        assert_eq!(wear_bucket(0.0), 0);
        assert_eq!(wear_bucket(1.0), WEAR_BUCKETS - 1);
        assert_eq!(wear_bucket(-0.5), 0, "clamped");
        assert_eq!(wear_bucket(1.5), WEAR_BUCKETS - 1, "clamped");
        for i in 0..100 {
            let w = i as f32 / 99.0;
            let b = wear_bucket(w);
            assert!(b < WEAR_BUCKETS);
            assert!(wear_bucket((w + 0.01).min(1.0)) >= b, "monotone");
        }
    }

    #[test]
    fn building_wear_is_deterministic_and_in_range() {
        // Same seed -> same wear per building (R3.6 requirement), and the
        // planned levels actually spread across the range.
        let graph = crate::world::path_network::generate_world(SEEDS[2])
            .expect("generation succeeds")
            .1;
        let wear_of = |seed: u64| -> Vec<f32> {
            let mut wear = Vec::new();
            // Sample every chunk of the map: the world is small (16x16), so a
            // centre-window like the old 16..24 leaves too few buildings.
            let cells = crate::world::GRID_CELLS as i32;
            for cx in 0..cells {
                for cz in 0..cells {
                    for o in &plan_chunk(&graph, seed, (cx, cz), 0).objects {
                        if o.kind == PlacedKind::Building(BlockType::OldCity)
                            || o.kind == PlacedKind::Building(BlockType::Apartment)
                            || o.kind == PlacedKind::Building(BlockType::Factory)
                        {
                            wear.push(o.wear);
                        }
                    }
                }
            }
            wear
        };
        let a = wear_of(SEEDS[2]);
        let b = wear_of(SEEDS[2]);
        assert_eq!(a, b, "bit-exact wear per seed");
        assert!(a.len() > 3, "sampled a real neighbourhood: {}", a.len());
        assert!(a.iter().all(|w| (0.0..=1.0).contains(w)));
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for w in &a {
            lo = lo.min(*w);
            hi = hi.max(*w);
        }
        assert!(hi - lo > 0.5, "wear spreads across the range: {lo}..{hi}");
    }

    #[test]
    fn rotated_corners_match_a_y_rotation() {
        // A 90° yaw swaps the footprint axes, and the corner set stays a
        // congruent rectangle around the center.
        let center = bevy::math::Vec2::new(10.0, 20.0);
        let corners = rotated_corners(center, std::f32::consts::FRAC_PI_2, 4.0, 2.0);
        let min = corners
            .iter()
            .fold(bevy::math::Vec2::splat(f32::INFINITY), |a, b| a.min(*b));
        let max = corners
            .iter()
            .fold(bevy::math::Vec2::splat(f32::NEG_INFINITY), |a, b| a.max(*b));
        let size = max - min;
        assert!((size.x - 2.0).abs() < 1e-4, "x extent {}", size.x);
        assert!((size.y - 4.0).abs() < 1e-4, "z extent {}", size.y);
    }

    /// Every building's FRONTAGE must be a whole number of facade bays.
    ///
    /// This is what stops the wall texture from cutting a window in half at a
    /// corner. With `yaw = atan2(-dir.z, dir.x)` the frontage is `size.x` (the
    /// ADR's "length along the edge"), so the assertion is on `size.x` —
    /// asserting on the other axis would test the party wall instead and pass
    /// while every street facade was still sliced.
    #[test]
    fn frontages_are_whole_bays() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut checked = 0;
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    let PlacedKind::Building(kind) = object.kind else {
                        continue;
                    };
                    let Some(style) = FacadeStyle::of(kind) else {
                        continue;
                    };
                    let bay = style.dims().bay;
                    let ratio = object.size.x / bay;
                    assert!(
                        (ratio - ratio.round()).abs() < 1e-3,
                        "seed {seed}: {} frontage {:.2} m is not a whole multiple of the {bay} m bay ({ratio:.3} bays)",
                        style.label(),
                        object.size.x
                    );
                    checked += 1;
                }
            }
            assert!(checked > 0, "seed {seed}: no buildings checked");
        }
    }

    /// Heights are whole storeys, and the old city keeps at least two distinct
    /// ones. The second half is the regression: a `ceil` snap turned all 1219
    /// old-city buildings into exactly 6 m, flattening the roofline ADR-0001
    /// is built on, and a count-only test would not have noticed.
    #[test]
    fn heights_snap_to_storeys_without_flattening_the_skyline() {
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            let mut storeys_seen = std::collections::BTreeSet::new();
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    let PlacedKind::Building(kind) = object.kind else {
                        continue;
                    };
                    let Some(style) = FacadeStyle::of(kind) else {
                        continue;
                    };
                    let floor = style.dims().floor;
                    let ratio = object.size.y / floor;
                    assert!(
                        (ratio - ratio.round()).abs() < 1e-3,
                        "seed {seed}: height {:.2} m is not a whole number of {floor} m storeys",
                        object.size.y
                    );
                    if kind == BlockType::OldCity {
                        storeys_seen.insert(ratio.round() as i32);
                    }
                }
            }
            assert!(
                storeys_seen.len() >= 2,
                "seed {seed}: every old-city building is {} storey(s); the roofline is flat",
                storeys_seen.iter().next().copied().unwrap_or(0)
            );
        }
    }

    /// The variant must be stable for one position and spread over the whole
    /// range, or "two neighbours rarely share a tile" is false.
    #[test]
    fn facade_variants_are_stable_and_spread() {
        let mut counts = [0usize; VARIANT_COUNT as usize];
        for seed in SEEDS {
            let (_, graph) = generate_world(seed).unwrap();
            for chunk in all_chunks(&graph) {
                for object in plan_chunk(&graph, seed, chunk, 3).objects {
                    if !matches!(object.kind, PlacedKind::Building(_)) {
                        continue;
                    }
                    assert_eq!(
                        object.variant,
                        facade_variant(object.position),
                        "the planned variant must be the position hash"
                    );
                    counts[object.variant as usize] += 1;
                }
            }
        }
        let total: usize = counts.iter().sum();
        for (variant, count) in counts.iter().enumerate() {
            let share = *count as f32 / total as f32;
            assert!(
                (0.25..0.75).contains(&share),
                "variant {variant} covers {:.0}% of the city; the split is lopsided",
                share * 100.0
            );
        }
    }

    const GRAPH_LAST_CHUNK: i32 = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32 - 1;
}
