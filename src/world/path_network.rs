//! Block-first world generation (ADR-0002/0004, ported from the original
//! M2/M3 algorithms).
//!
//! Algorithm: the map is partitioned into rectangular blocks (merged cell
//! groups, block-first — blocks are the positive shape, streets are the
//! negative space between them). Street unit edges are the grid lines between
//! blocks, minus the lines between adjacent outer-band cells (the enclosure
//! band is never crossed by streets). Full partitioning plus the band rule
//! guarantees every street forms a cycle: with a complete rectangle
//! partition, an interior lattice vertex cannot have street degree 1, and
//! border vertices only see band-internal (excluded) edges — so there are no
//! dead ends. Both properties are asserted by unit tests.
//!
//! Determinism: a single `rand_chacha::StdRng::seed_from_u64(seed)` drives
//! every decision in a fixed cell-scan order; `thread_rng` never enters the
//! generation path. Same seed → bit-exact world (unit tested).

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use bevy::ecs::prelude::Resource;
use bevy::math::Vec2;

use crate::core::error::{GameError, GameResult};
use crate::world::geometry::inset_polygon_clip_with_insets;
use crate::world::layout_graph::{
    BlockType, EdgeKind, LayoutBlock, LayoutGraph, NodeKind, NodeVertex, StreetUnitEdge,
};
use crate::world::world_grid::WorldGrid;
use crate::world::{
    ARTERIAL_WIDTH_MAX, ARTERIAL_WIDTH_MIN, CELL_SIZE, FOREST_EDGE_MAX_DISTANCE_CELLS, GRID_CELLS,
    OUTER_BAND_CELLS, PERTURB_MAX, STREET_WIDTH_MAX, STREET_WIDTH_MIN, VILLAGE_STREET_WIDTH_MAX,
    VILLAGE_STREET_WIDTH_MIN,
};

/// The save seed driving generation (ADR-0002: one save = one seed = one
/// map). Inserted before adding the world plugin (R4 menu provides it);
/// defaults to [`DEFAULT_SEED`] in dev builds until then.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorldSeed(pub u64);

/// Block id used for cells that belong to no block: the scenery band behind
/// the ring road (the outermost cell ring).
pub const SCENERY_BAND_CELL: u32 = u32::MAX;

/// Type selection weights (port of the original `_assign_interior_type`).
///
/// Interior cells: PLAZA 6%, FOREST 14% (edge transition only), then
/// URBAN_VILLAGE 15% (R3.20d 2026-11-19 user review "红色老城区占比还是太高
/// 了": raised from 10% — the 城中村 texture should read stronger), and
/// the remaining 65% split OLD_CITY 80 / APARTMENT 20 of the built-up
/// remainder (R3.20b: the factory share moved into the map-level factory
/// reservation; R3.20d: apartment doubled on the same review).
///
/// R3.20 (2026-11-19 user decision: 同类连片): the per-roll probabilities are
/// the per-CELL targets above divided by each type's expected district area,
/// because one roll now claims a whole district rectangle, not one cell.
/// Expected areas: plaza 1x1 = 1, forest 2-3 x 2-3 = 6.25, village
/// 1-2 x 2-3 = 3.75, old city 1-2 x 1-2 = 2.25, apartment 2-3 square =
/// 6.25. Normalizing share/area reproduces the AREA shares the ADR-0001 mix
/// asks for: cumulative thresholds 0.1531 / 0.2102 / 0.332 / 0.80-built.
const P_PLAZA_END: f32 = 0.1531;
const P_FOREST_END: f32 = 0.2102;
/// 城中村 share of interior cells: 15% of area (R3.20d 2026-11-19 user
/// review "红色老城区占比还是太高了", raised from 10%). Villages grow into
/// 2-6 cell rectangles (never an isolated 1x1), so a district roll is rarer
/// than the old per-cell roll: p = 0.15 x 3.05 / 3.75 on top of the forest
/// threshold (K = 3.05 = expected built district area at the 80/20 split).
const P_VILLAGE_END: f32 = 0.332;
/// Built-up split of the fill scan (ADR-0001 "风格比例约 50/30/20"): R3.20b
/// removed the fill-roll factory arm (factories come solely from the
/// map-level reservation); R3.20d doubled the apartment share on the same
/// review — 80/20 old/apartment.
const P_OLD_AREA: f32 = 0.80;

/// Generates the complete world layout for `seed`.
///
/// Runs at startup in well under 1 s (pure CPU over a 40x40 grid, no mesh
/// data). Returns the serde-friendly [`WorldGrid`] plus the [`LayoutGraph`].
pub fn generate_world(seed: u64) -> GameResult<(WorldGrid, LayoutGraph)> {
    let mut rng = StdRng::seed_from_u64(seed);
    // ADR-0009: the arterial cross is the first tier-0 decision, drawn before
    // anything else so the rest of the stream stays deterministic behind it.
    let cross = arterial_cross(&mut rng);
    // R2.9: perturbed lattice nodes are the single vertex source for every
    // block polygon (original `_generate_nodes`). Generated first so block
    // polygons can follow the real (wobbly) street centerlines.
    let nodes = perturbed_nodes(&mut rng, cross);
    let (blocks, mut grid) = partition_blocks(&mut rng, &nodes, cross)?;
    let unit_edges = street_unit_edges(&grid, &blocks, cross);
    let node_vertices = node_vertices(&unit_edges);
    mark_grid_flags(&mut grid, &unit_edges, &node_vertices)?;
    let mut graph = LayoutGraph::from_generation(blocks, &unit_edges, &node_vertices, &nodes);
    associate_edges_with_blocks(&mut graph, &unit_edges, &grid, &nodes);
    // ADR-0008 tier-1 pass: lanes and sub-blocks inside built blocks. Runs
    // on its own RNG so every tier-0 decision above stays bit-exact, and
    // AFTER edge association so lane mouths can find their street edges.
    crate::world::subdivision::subdivide_blocks(&mut graph, seed);
    grid.node_positions = nodes;
    Ok((grid, graph))
}

/// Lattice vertices per axis: `GRID_CELLS + 1` nodes span the cell grid.
pub const NODES_PER_AXIS: u32 = GRID_CELLS + 1;

/// The tier-0 arterial cross (ADR-0009): one vertical and one horizontal
/// 10-15 m road that split the map into four quadrants. The cross sits on
/// lattice lines (never perturbed), keeps its drawn width along its whole
/// run, and every seam between two quadrants IS the arterial — blocks never
/// span it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ArterialCross {
    /// Lattice column of the vertical arterial (7 or 8).
    pub col: u32,
    /// Lattice row of the horizontal arterial (7 or 8).
    pub row: u32,
    /// Width of the vertical arterial (m).
    pub width_v: f32,
    /// Width of the horizontal arterial (m).
    pub width_h: f32,
}

/// Draws the arterial cross. Fixed draw order (col, row, width_v, width_h)
/// pins the RNG stream: everything downstream is deterministic behind it.
///
/// Jitter `{7, 8}` keeps the cross within one cell of the map centre (the
/// lattice lines at 350 m and 400 m), so every quadrant keeps at least
/// 6 cells = 300 m and a 5x5 factory (250 m) never has to degrade. A wider
/// jitter would reintroduce clamped-merge noise at the quadrant edges.
pub(crate) fn arterial_cross(rng: &mut StdRng) -> ArterialCross {
    let col = rng.random_range(7..=8);
    let row = rng.random_range(7..=8);
    let width_v = rng.random_range(ARTERIAL_WIDTH_MIN..=ARTERIAL_WIDTH_MAX);
    let width_h = rng.random_range(ARTERIAL_WIDTH_MIN..=ARTERIAL_WIDTH_MAX);
    ArterialCross {
        col,
        row,
        width_v,
        width_h,
    }
}

/// Generates every lattice node, perturbed by up to [`PERTURB_MAX`] on each
/// axis (port of the original `_generate_nodes`).
///
/// Index = `row * NODES_PER_AXIS + col`. Map-border nodes are **not**
/// perturbed: they must stay exactly on `x/y = 0` and `WORLD_SIZE` so the
/// enclosure band closes flush against the world edge.
///
/// Draw order (one RNG, fixed scan): perturb_x, perturb_z, then a plaza roll
/// per interior node — identical to the original so the same seed reproduces
/// the same layout structure.
pub(crate) fn perturbed_nodes(rng: &mut StdRng, cross: ArterialCross) -> Vec<Vec2> {
    let axis = NODES_PER_AXIS as usize;
    let mut nodes = Vec::with_capacity(axis * axis);
    for row in 0..axis {
        for col in 0..axis {
            let base_x = col as f32 * CELL_SIZE;
            let base_z = row as f32 * CELL_SIZE;
            let mut perturb_x = 0.0;
            let mut perturb_z = 0.0;
            // Border nodes are pinned on BOTH axes (the original's comment
            // says "边界节点不扰动"; its per-axis guards let a node on the top
            // edge still slide along x, which pushed outer block edges off the
            // map border and left gaps in the enclosure wall).
            let on_border = row == 0 || col == 0 || row == axis - 1 || col == axis - 1;
            // The first interior lattice line on each axis is the ring
            // road's centerline (2026-10-21 boundary decision: the loop runs
            // straight along lattice points). Perturbing it lets ring-side
            // blocks inset from a shifted node and encroach on the loop's
            // inner half — measured down to a 0.3 m free lane and negative
            // lanes on other seeds (R3.8 walk test).
            let on_ring_line = row == 1 || col == 1 || row == axis - 2 || col == axis - 2;
            // ADR-0009: the arterial cross owns its two lattice lines whole —
            // a wobble there would bend the 10-15 m main road and misalign
            // the quadrant-seam insets. Pinned like the ring.
            let on_arterial = col == cross.col as usize || row == cross.row as usize;
            if !on_border && !on_ring_line && !on_arterial {
                perturb_x = rng.random_range(-PERTURB_MAX..=PERTURB_MAX);
                perturb_z = rng.random_range(-PERTURB_MAX..=PERTURB_MAX);
            } else {
                // Consume the same two draws so the RNG stream stays aligned
                // with the original regardless of border handling.
                let _ = rng.random::<f32>();
                let _ = rng.random::<f32>();
            }
            // Reserved plaza roll (original marks a visual hint node; the
            // project's plaza anchors come from PLAZA blocks instead, so the
            // draw is consumed only to keep the RNG stream aligned).
            let _ = nodes.len();
            if row > 0 && row < axis - 1 && col > 0 && col < axis - 1 {
                let _is_plaza_hint = rng.random::<f32>() < 0.08;
                let _ = _is_plaza_hint;
            }
            nodes.push(Vec2::new(base_x + perturb_x, base_z + perturb_z));
        }
    }
    nodes
}

/// Partitions the interior grid into a complete, non-overlapping rectangle
/// tiling (user decision 2026-10-21: "完整矩形剖分").
///
/// Why a tiling rather than carve-and-fallback: the previous approach scanned
/// cells and carved N×N when it fit, leaving L-shaped remnants where a carve
/// was blocked. Those remnants are not rectangles, so their perimeter polygon
/// walked a reflex corner that folded inside-out under inset — 280 pairs per
/// world overlapped by up to 20 m. A tiling has no remnants by construction:
/// every cell belongs to exactly one rectangle, so every block polygon is a
/// convex quad standing on its own perimeter of lattice nodes, and insetting
/// each block by its own half width can never make two of them meet.
///
/// Algorithm (ADR-0009): the interior is split into four quadrants by the
/// arterial cross; each quadrant is processed (order NW, NE, SW, SE) with the
/// same deterministic steps:
/// 0. R3.20b/c: reserve ONE 4-5² factory and ONE 2-3² apartment district
///    for the whole map, each centred in a randomly drawn (distinct)
///    quadrant — the fill scan fragments free space too much for a
///    factory roll to ever fit, and the same fragmentation regularly
///    ate the apartment quota too;
/// 1. take the first unassigned cell;
/// 2. R3.20 同类连片: offer it to a same-kind village/forest/old-city
///    district to the left or above — rectangle-preserving extensions chain
///    contiguous districts out of single rolls;
/// 3. otherwise roll a district type and grow its rectangle
///    ([`district_size`]) inside the quadrant (the scan range stops at the
///    arterial lines, so no block spans the cross);
/// 4. undersized fits degrade to a 1x1 OLD_CITY (the dense low-rise filler).
///
/// Phase 2 builds every block's inset polygon once the scan is final, so an
/// extended district still gets one polygon from its whole lattice rect.
/// Same-kind seams inset by the narrow [`seam_width`] instead of the street
/// average — districts stay visually contiguous even where an extension was
/// declined.
fn partition_blocks(
    rng: &mut StdRng,
    nodes: &[Vec2],
    cross: ArterialCross,
) -> GameResult<(Vec<LayoutBlock>, WorldGrid)> {
    let width = GRID_CELLS;
    let height = GRID_CELLS;
    let mut grid = WorldGrid::filled(width, height, SCENERY_BAND_CELL);
    let mut owner: Vec<Option<u32>> = vec![None; (width * height) as usize];
    // R3.20: lattice rect (cell rows/cols; r1/c1 EXCLUSIVE) per block id,
    // parallel to `blocks`. Extensions grow the rect; the inset polygons are
    // built in phase 2 once every district is final.
    let mut rects: Vec<(u32, u32, u32, u32)> = Vec::new();
    let mut blocks: Vec<LayoutBlock> = Vec::new();

    // Interior region: everything except the scenery ring. The arterial lines
    // split it into four quadrant scans (ADR-0009); lattice line `cross.col`
    // separates cell columns `cross.col - 1` (west) and `cross.col` (east),
    // so the west quadrants end at `cross.col` and the east ones start there.
    let inner_start = OUTER_BAND_CELLS;
    let inner_end_row = height - OUTER_BAND_CELLS;
    let inner_end_col = width - OUTER_BAND_CELLS;

    // R3.20b/c reserved districts: claim ONE factory AND ONE apartment
    // district for the whole map (2026-11-19 user review: the fragmented
    // fill scan could not fit a 4-5² factory — and often not a 2-3²
    // apartment either — so every roll degraded to old city and some maps
    // showed no factory at all while the showcase seed lost its blue
    // apartment estate entirely), before the fill scan. Each is centred in
    // its own drawn quadrant (quadrants are 6-8 cells per side, so any
    // district size fits an empty one), keeping its distance from both the
    // arterial cross and the ring road; the fill scan works around them.
    // The scan's factory arm is gone (the reservation is the sole factory
    // source) while apartment rolls can still chain more blue districts.
    let (factory_req_w, factory_req_h) = district_size(rng, BlockType::Factory);
    let (apartment_req_w, apartment_req_h) = district_size(rng, BlockType::Apartment);
    let factory_quadrant = rng.random_range(0..4_usize);
    // Distinct host quadrants (a centred 5² factory leaves no room for a
    // centred 3² apartment in the same 6-8 cell quadrant). Deterministic
    // shift, one draw.
    let apartment_quadrant = (factory_quadrant + 1 + rng.random_range(0..3_usize)) % 4;

    for (row_band, &(row_start, row_end)) in [(inner_start, cross.row), (cross.row, inner_end_row)]
        .iter()
        .enumerate()
    {
        for (col_band, &(col_start, col_end)) in
            [(inner_start, cross.col), (cross.col, inner_end_col)]
                .iter()
                .enumerate()
        {
            // NW=0, NE=1, SW=2, SE=3 — the iteration order of the fill scan.
            let quadrant = row_band * 2 + col_band;
            if quadrant == factory_quadrant {
                place_reserved_district(
                    &mut owner,
                    width,
                    &mut rects,
                    &mut blocks,
                    rng,
                    BlockType::Factory,
                    factory_req_w,
                    factory_req_h,
                    4,
                    row_start,
                    row_end,
                    col_start,
                    col_end,
                );
            }
            if quadrant == apartment_quadrant {
                place_reserved_district(
                    &mut owner,
                    width,
                    &mut rects,
                    &mut blocks,
                    rng,
                    BlockType::Apartment,
                    apartment_req_w,
                    apartment_req_h,
                    2,
                    row_start,
                    row_end,
                    col_start,
                    col_end,
                );
            }
            for row in row_start..row_end {
                for col in col_start..col_end {
                    if owner[(row * width + col) as usize].is_some() {
                        continue;
                    }

                    // R3.20 district growth (2026-11-19 user decision: 同类
                    // 连片): before rolling a new district, offer the free
                    // cell to a same-kind neighbour — contiguous village,
                    // forest and old-city textures form by absorption.
                    if try_extend_district(
                        &mut owner,
                        width,
                        &mut rects,
                        &mut blocks,
                        row,
                        col,
                        row_start,
                        col_start,
                        rng,
                    ) {
                        continue;
                    }

                    let distance_to_border =
                        row.min(col).min(height - 1 - row).min(width - 1 - col);

                    // Area-aware type pick. A flat per-block roll makes factories
                    // dominate the map: 8% of blocks but ~50% of the area, because a
                    // factory eats 16-25 cells at once (ADR-0001 wants a 50/30/20
                    // old-city / apartment / factory look, which is what the player
                    // sees as area, not as block count). So biased sampling: the
                    // bigger the footprint a type would claim, the lower its
                    // probability at a fixed area quota.
                    let roll: f32 = rng.random();
                    let mut kind = if roll < P_PLAZA_END {
                        BlockType::Plaza
                    } else if roll < P_FOREST_END {
                        BlockType::Forest
                    } else if roll < P_VILLAGE_END {
                        BlockType::UrbanVillage
                    } else {
                        // R3.20b: no factory arm here any more — factories
                        // come solely from the map-level reservation, which
                        // is the only placement that can guarantee a
                        // contiguous 4-5² rect (the fragmented fill scan
                        // degraded every fill-roll factory to old city
                        // anyway). The old 3% factory share goes to old city.
                        let built_roll: f32 = rng.random();
                        if built_roll < P_OLD_AREA {
                            BlockType::OldCity
                        } else {
                            BlockType::Apartment
                        }
                    };
                    if kind == BlockType::Forest
                        && distance_to_border > FOREST_EDGE_MAX_DISTANCE_CELLS
                    {
                        kind = BlockType::OldCity;
                    }

                    let (req_w, req_h) = district_size(rng, kind);
                    let (mut w, mut h) =
                        largest_free_rect(&owner, width, row, col, row_end, col_end, req_w, req_h);
                    if w == 0 {
                        continue; // Cannot happen: the scan cell itself is free.
                    }
                    // Keep the type/merge contract: a block smaller than its type's
                    // minimum footprint degrades to OLD_CITY (the dense low-rise
                    // filler), not to a mislabelled undersized apartment/factory.
                    // R3.20: a village clamped to a single cell degrades too —
                    // 城中村 never appears as an isolated 1x1 block.
                    let undersized = match kind {
                        BlockType::Apartment => w < 2 || h < 2,
                        BlockType::Factory => w < 4 || h < 4,
                        BlockType::UrbanVillage => w * h < 2,
                        BlockType::OldCity | BlockType::Plaza | BlockType::Forest => false,
                    };
                    if undersized {
                        kind = BlockType::OldCity;
                        (w, h) = (1, 1);
                    }

                    let block_id = blocks.len() as u32;
                    // One draw either way, so the RNG stream keeps one street-width
                    // draw per district regardless of kind (2026-11: villages draw
                    // from their own narrower range — see VILLAGE_STREET_WIDTH_*).
                    let street_width = match kind {
                        BlockType::UrbanVillage => {
                            rng.random_range(VILLAGE_STREET_WIDTH_MIN..=VILLAGE_STREET_WIDTH_MAX)
                        }
                        _ => rng.random_range(STREET_WIDTH_MIN..=STREET_WIDTH_MAX),
                    };
                    let mut cells = Vec::with_capacity((w * h) as usize);
                    for r in row..(row + h) {
                        for c in col..(col + w) {
                            owner[(r * width + c) as usize] = Some(block_id);
                            cells.push((r, c));
                        }
                    }
                    rects.push((row, col, row + h, col + w));
                    blocks.push(LayoutBlock {
                        id: block_id,
                        kind,
                        cells,
                        street_width,
                        is_outer: false,
                        // R3.20 phase 2 fills the polygon once every block is
                        // final (extensions may still grow this one).
                        polygon: Vec::new(),
                        children: Vec::new(),
                    });
                }
            }
        }
    }

    // R3.20 phase 2: with the scan done every block is final, so the inset
    // polygons are built here from the lattice rects — one inset per unit
    // perimeter edge (ADR-0009 arterial rule + R3.20 same-kind seam rule).
    let polygons: Vec<Vec<Vec2>> = (0..blocks.len())
        .map(|index| {
            let (r0, c0, r1, c1) = rects[index];
            let perimeter = block_perimeter(r0, c0, r1 - r0, c1 - c0, nodes);
            let n = perimeter.len();
            let insets: Vec<f32> = (0..n)
                .map(|i| {
                    let (_, ra, ca) = perimeter[i];
                    let (_, rb, cb) = perimeter[(i + 1) % n];
                    edge_inset(ra, ca, rb, cb, r0, c0, &blocks, index, &owner, width, cross)
                })
                .collect();
            let vertices: Vec<Vec2> = perimeter.iter().map(|(p, _, _)| *p).collect();
            inset_polygon_clip_with_insets(&vertices, &insets).ok_or_else(|| {
                GameError::Generation(format!(
                    "block {index} polygon collapsed (width {})",
                    blocks[index].street_width
                ))
            })
        })
        .collect::<GameResult<_>>()?;
    for (block, polygon) in blocks.iter_mut().zip(polygons) {
        block.polygon = polygon;
    }

    for row in 0..height {
        for col in 0..width {
            let cell = grid.get_cell_mut(row, col)?;
            cell.block_id = owner[(row * width + col) as usize].unwrap_or(SCENERY_BAND_CELL);
        }
    }
    Ok((blocks, grid))
}

/// Probability that an extendable same-kind district absorbs the free cell
/// next to it (R3.20). Below 1.0 so districts keep some seams between them —
/// a map of a few mega-districts would read as stripes, not neighbourhoods.
const DISTRICT_EXTEND_PROB: f32 = 0.65;

/// Seam width of two same-kind old-city districts (R3.20): a narrow lane,
/// not the 6-10 m street two different blocks would get.
const OLD_CITY_SEAM_WIDTH: f32 = 4.0;

/// Seam width of two same-kind forest districts (R3.20): a footpath through
/// the trees, not a road.
const FOREST_PATH_WIDTH: f32 = 3.0;

/// Kinds whose districts may grow by absorbing adjacent free cells (R3.20):
/// the contiguous textures the design asks for. Apartments, factories and
/// plazas keep their fixed footprints.
fn extendable(kind: BlockType) -> bool {
    matches!(
        kind,
        BlockType::UrbanVillage | BlockType::Forest | BlockType::OldCity
    )
}

/// Places one reserved district (R3.20b/c) centred in the given quadrant,
/// claimed from the still-empty `owner` grid before the fill scan.
///
/// The defensive `min_side` check can only fail if the quadrant were smaller
/// than the request (impossible: quadrants are 6-8 cells, requests are ≤5),
/// but skipping a reservation costs nothing and cannot mislabel a block.
#[allow(clippy::too_many_arguments)]
fn place_reserved_district(
    owner: &mut [Option<u32>],
    width: u32,
    rects: &mut Vec<(u32, u32, u32, u32)>,
    blocks: &mut Vec<LayoutBlock>,
    rng: &mut StdRng,
    kind: BlockType,
    req_w: u32,
    req_h: u32,
    min_side: u32,
    row_start: u32,
    row_end: u32,
    col_start: u32,
    col_end: u32,
) {
    let quadrant_h = row_end - row_start;
    let quadrant_w = col_end - col_start;
    let row = row_start + (quadrant_h - req_h) / 2;
    let col = col_start + (quadrant_w - req_w) / 2;
    let (w, h) = largest_free_rect(owner, width, row, col, row_end, col_end, req_w, req_h);
    if w < min_side || h < min_side {
        return;
    }
    let block_id = blocks.len() as u32;
    let street_width = rng.random_range(STREET_WIDTH_MIN..=STREET_WIDTH_MAX);
    let mut cells = Vec::with_capacity((w * h) as usize);
    for r in row..(row + h) {
        for c in col..(col + w) {
            owner[(r * width + c) as usize] = Some(block_id);
            cells.push((r, c));
        }
    }
    rects.push((row, col, row + h, col + w));
    blocks.push(LayoutBlock {
        id: block_id,
        kind,
        cells,
        street_width,
        is_outer: false,
        polygon: Vec::new(),
        children: Vec::new(),
    });
}

/// Tries to absorb the free cell `(row, col)` into a same-kind district to
/// its left or above (R3.20 同类连片). Returns true when the cell was absorbed.
///
/// Only rectangle-preserving extensions are taken: the cell must sit exactly
/// at the neighbour's right edge (within its row span) or below its bottom
/// edge (within its col span), and the whole strip between must be free —
/// blocks stay rectangles, which the complete-partition inset guarantee
/// needs (an L-shaped district would fold inside-out under inset).
///
/// Scan-order note: the row-major scan reaches a cell after everything to
/// its left and above, so those are the only neighbours that can own a
/// district yet. Extensions never cross the quadrant scan bounds, so no
/// district ever spans the arterial cross.
#[allow(clippy::too_many_arguments)]
fn try_extend_district(
    owner: &mut [Option<u32>],
    width: u32,
    rects: &mut [(u32, u32, u32, u32)],
    blocks: &mut [LayoutBlock],
    row: u32,
    col: u32,
    row_start: u32,
    col_start: u32,
    rng: &mut StdRng,
) -> bool {
    // Candidate A: the block to the left, extended east.
    let left = (col > col_start)
        .then(|| owner[(row * width + col - 1) as usize])
        .flatten()
        .filter(|&id| {
            let (r0, _c0, r1, c1) = rects[id as usize];
            extendable(blocks[id as usize].kind)
                && row >= r0
                && row < r1
                && col == c1
                && (r0..r1).all(|r| owner[(r * width + col) as usize].is_none())
        });
    // Candidate B: the block above, extended south.
    let up = (row > row_start)
        .then(|| owner[((row - 1) * width + col) as usize])
        .flatten()
        .filter(|&id| {
            let (_r0, c0, r1, c1) = rects[id as usize];
            extendable(blocks[id as usize].kind)
                && col >= c0
                && col < c1
                && row == r1
                && (c0..c1).all(|c| owner[(row * width + c) as usize].is_none())
        });
    // The two candidates are mutually exclusive (A needs row < r1, B needs
    // col < c1, and the corner cell satisfies neither), but a cell can have
    // both a left and an up district — pick one with a coin flip.
    let chosen = match (left, up) {
        (Some(a), Some(b)) => {
            if rng.random_bool(0.5) {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return false,
    };
    if !rng.random_bool(f64::from(DISTRICT_EXTEND_PROB)) {
        return false;
    }
    let id = chosen as usize;
    let (r0, c0, r1, c1) = rects[id];
    // The strip the validated candidate claims: east (rows r0..r1 at col) or
    // south (cols c0..c1 at row) — which one follows from the same geometry
    // the validation used.
    let east = row >= r0 && row < r1 && col == c1;
    let strip: Vec<(u32, u32)> = if east {
        (r0..r1).map(|r| (r, col)).collect()
    } else {
        (c0..c1).map(|c| (row, c)).collect()
    };
    for &(r, c) in &strip {
        owner[(r * width + c) as usize] = Some(chosen);
        blocks[id].cells.push((r, c));
    }
    rects[id] = (r0, c0, r1.max(row + 1), c1.max(col + 1));
    true
}

/// Largest rectangle (cols <= `req_w`, rows <= `req_h`) that fits at
/// `(row, col)` inside the quadrant scan range and is entirely unassigned.
/// Maximizes area, scanning heights descending then widths descending so the
/// choice is deterministic; `(0, _)` means even one cell could not fit
/// (impossible for the scan cell itself, kept for symmetry).
#[allow(clippy::too_many_arguments)]
fn largest_free_rect(
    owner: &[Option<u32>],
    width: u32,
    row: u32,
    col: u32,
    end_row: u32,
    end_col: u32,
    req_w: u32,
    req_h: u32,
) -> (u32, u32) {
    let mut best = (0u32, 0u32);
    let mut best_area = 0u32;
    for h in (1..=req_h.min(end_row - row)).rev() {
        for w in (1..=req_w.min(end_col - col)).rev() {
            if w * h <= best_area {
                break;
            }
            let fits = (row..row + h)
                .all(|r| (col..col + w).all(|c| owner[(r * width + c) as usize].is_none()));
            if fits {
                best = (w, h);
                best_area = w * h;
            }
        }
    }
    best
}

/// Seam width between two blocks facing each other across a lattice line
/// (R3.20): the average of the two street widths, except same-kind
/// village/old-city/forest districts, whose seam is downgraded to a narrow
/// lane (2026-11-19 user decision: 同类地块相邻时内部道路降级——连片的住宅区/
/// 森林/城中村之间只留巷道). Both flanks inset by half of THIS width, so
/// their polygons still meet exactly on the centerline.
fn seam_width(a: &LayoutBlock, b: &LayoutBlock) -> f32 {
    if a.kind == b.kind {
        match a.kind {
            BlockType::UrbanVillage => a.street_width.min(b.street_width),
            BlockType::OldCity => OLD_CITY_SEAM_WIDTH,
            BlockType::Forest => FOREST_PATH_WIDTH,
            _ => (a.street_width + b.street_width) * 0.5,
        }
    } else {
        (a.street_width + b.street_width) * 0.5
    }
}

/// Half-width inset for one unit perimeter edge of block `index` (R3.19
/// per-edge insets; R3.20 adds the same-kind seam downgrade).
///
/// - an edge on the arterial lines retreats by the arterial half width (the
///   seam there IS the arterial, not the block's own street);
/// - an edge facing a same-kind village/old-city/forest district retreats by
///   half the narrow seam width from [`seam_width`];
/// - every other edge retreats by the block's own half street width.
#[allow(clippy::too_many_arguments)]
fn edge_inset(
    ra: u32,
    ca: u32,
    rb: u32,
    cb: u32,
    r0: u32,
    c0: u32,
    blocks: &[LayoutBlock],
    index: usize,
    owner: &[Option<u32>],
    width: u32,
    cross: ArterialCross,
) -> f32 {
    if ca == cb && ca == cross.col {
        return cross.width_v * 0.5;
    }
    if ra == rb && ra == cross.row {
        return cross.width_h * 0.5;
    }
    let half = blocks[index].street_width * 0.5;
    // The neighbour cell across this unit edge: a horizontal edge lies on the
    // block's top (r == r0) or bottom (r == r1) lattice line, a vertical edge
    // on its left (c == c0) or right (c == c1) line. Unit edges have exactly
    // one flanking cell per side.
    let (nr, nc) = if ra == rb {
        if ra == r0 { (ra - 1, ca) } else { (ra, ca) }
    } else if ca == c0 {
        (ra, ca - 1)
    } else {
        (ra, ca)
    };
    match owner[(nr * width + nc) as usize] {
        Some(id) if id as usize != index => seam_width(&blocks[id as usize], &blocks[index]) * 0.5,
        _ => half,
    }
}

/// District footprint per type (R3.20, replacing ADR-0004's square-only
/// merge table). Draws `(cols, rows)` in that fixed order, so the RNG stream
/// stays deterministic behind it.
///
/// - OLD_CITY 1-2 x 1-2, URBAN_VILLAGE 1-2 x 2-3 (area >= 2 by construction,
///   so a village never spawns as an isolated 1x1 block — 2026-11-19 user
///   rule), FOREST 2-3 x 2-3, PLAZA 1x1;
/// - APARTMENT 2x2 or 3x3, FACTORY 4x4 or 5x5 (random, as before).
///
/// Extensions then chain further same-kind cells onto these rectangles, so
/// villages/forests/old city read as contiguous districts rather than a
/// scatter of 50 m blocks.
fn district_size(rng: &mut StdRng, kind: BlockType) -> (u32, u32) {
    match kind {
        BlockType::Apartment => {
            let side = if rng.random_bool(0.5) { 2 } else { 3 };
            (side, side)
        }
        BlockType::Factory => {
            let side = if rng.random_bool(0.5) { 4 } else { 5 };
            (side, side)
        }
        BlockType::OldCity => (rng.random_range(1..=2), rng.random_range(1..=2)),
        BlockType::UrbanVillage => (rng.random_range(1..=2), rng.random_range(2..=3)),
        BlockType::Forest => (rng.random_range(2..=3), rng.random_range(2..=3)),
        BlockType::Plaza => (1, 1),
    }
}

/// Node position by lattice coordinates.
fn node_at(nodes: &[Vec2], row: u32, col: u32) -> Vec2 {
    nodes[(row * NODES_PER_AXIS + col) as usize]
}

/// Builds a block's polygon by walking its **full perimeter** of lattice
/// nodes (port of the original `_build_block_polygon`, 2026-08-26 fix).
///
/// Taking only the 4 corner nodes would make a merged N×N block's straight
/// edges ignore the perturbed nodes in between, misaligning it against the
/// small neighbor blocks that do follow them — the original measured up to
/// **7.47 m** of overlap from exactly that bug. Order: top → right → bottom
/// → left, giving `(n + 1) * 4` vertices standing on the shared street
/// centerline.
///
/// Each vertex carries its lattice coordinates `(row, col)` so the caller
/// can tell which perimeter edges lie on the arterial lines and inset them
/// by the arterial half width (ADR-0009).
fn block_perimeter(
    row: u32,
    col: u32,
    rows: u32,
    cols: u32,
    nodes: &[Vec2],
) -> Vec<(Vec2, u32, u32)> {
    let mut poly = Vec::with_capacity((2 * (rows + cols)) as usize);
    let last_row = row + rows;
    let last_col = col + cols;
    // Top: (row, col..=last_col)
    for c in col..=last_col {
        poly.push((node_at(nodes, row, c), row, c));
    }
    // Right: (row+1..=last_row, last_col)
    for r in (row + 1)..=last_row {
        poly.push((node_at(nodes, r, last_col), r, last_col));
    }
    // Bottom: (last_row, last_col-1..col)
    for c in (col..last_col).rev() {
        poly.push((node_at(nodes, last_row, c), last_row, c));
    }
    // Left: (last_row-1..=row+1, col)
    for r in ((row + 1)..last_row).rev() {
        poly.push((node_at(nodes, r, col), r, col));
    }
    poly
}

/// Street unit edges: grid lines between cells of different blocks, excluding
/// lines between two outer-band cells (the enclosure band has no internal
/// streets — the band rule that guarantees no dead ends).
///
/// Edge width is [`seam_width`] of the flanking blocks: the average of the
/// two street widths (each block insets by width / 2 on its own side of the
/// shared centerline), except the arterial cross (ADR-0009: a unit edge on
/// the vertical line `cross.col` or the horizontal line `cross.row` carries
/// the arterial's own drawn width and `EdgeKind::Arterial`) and R3.20's
/// same-kind district seams, which downgrade to narrow lanes.
fn street_unit_edges(
    grid: &WorldGrid,
    blocks: &[LayoutBlock],
    cross: ArterialCross,
) -> Vec<StreetUnitEdge> {
    let width = grid.width;
    let height = grid.height;
    let block_of = |row: u32, col: u32| grid.get_cell(row, col).expect("in bounds").block_id;
    let mut edges = Vec::new();
    for row in 0..height {
        for col in 0..width {
            for (nr, nc) in [
                (row, col + 1), // right neighbor
                (row + 1, col), // bottom neighbor
            ] {
                if nr >= height || nc >= width {
                    continue;
                }
                let a = block_of(row, col);
                let b = block_of(nr, nc);
                // Two scenery cells: no street behind the ring road.
                if a == SCENERY_BAND_CELL && b == SCENERY_BAND_CELL {
                    continue;
                }
                // Scenery next to a block: that seam IS the ring road, so the
                // ring gets a straight centerline on the interior lattice line.
                // Its width comes from the block alone (the band has no width).
                let edge_width = match (a == SCENERY_BAND_CELL, b == SCENERY_BAND_CELL) {
                    (false, false) if a == b => continue, // merged block interior
                    (false, false) => seam_width(&blocks[a as usize], &blocks[b as usize]),
                    (true, false) => blocks[b as usize].street_width,
                    (false, true) => blocks[a as usize].street_width,
                    (true, true) => unreachable!(),
                };
                let vertices = if nr == row {
                    // Side-by-side cells: vertical line at col = max.
                    let c = col.max(nc);
                    ((row, c), (row + 1, c))
                } else {
                    // Stacked cells: horizontal line at row = max.
                    let r = row.max(nr);
                    ((r, col), (r, col + 1))
                };
                // ADR-0009: a seam on the arterial lines IS the arterial — its
                // width comes from the cross, never from the flanking blocks
                // (whose insets match the arterial half width on that side).
                let on_arterial_v = vertices.0.1 == vertices.1.1 && vertices.0.1 == cross.col;
                let on_arterial_h = vertices.0.0 == vertices.1.0 && vertices.0.0 == cross.row;
                let (edge_width, kind) = if on_arterial_v {
                    (cross.width_v, EdgeKind::Arterial)
                } else if on_arterial_h {
                    (cross.width_h, EdgeKind::Arterial)
                } else {
                    (edge_width, EdgeKind::Street)
                };
                edges.push(StreetUnitEdge {
                    vertices,
                    width: edge_width,
                    kind,
                });
            }
        }
    }
    edges
}

/// Street degree per lattice vertex, from the unit edges.
fn vertex_degrees(unit_edges: &[StreetUnitEdge]) -> Vec<Vec<u8>> {
    let mut degrees = vec![vec![0u8; GRID_CELLS as usize + 1]; GRID_CELLS as usize + 1];
    for edge in unit_edges {
        let ((r0, c0), (r1, c1)) = edge.vertices;
        degrees[r0 as usize][c0 as usize] += 1;
        degrees[r1 as usize][c1 as usize] += 1;
    }
    degrees
}

/// Lattice vertices hosting graph nodes: junctions (degree >= 3), corners
/// (degree 2 with perpendicular edges) and — since R3.20 — straight
/// degree-2 vertices ([`NodeKind::Pass`]). Every unit-edge endpoint must
/// host a node: merged 2+-cell districts leave their interior lattice
/// vertices without a crossing street (degree 2 on one line only), and
/// skipping those nodes dropped the unit edges behind them, punching 50-100 m
/// holes into the ring and district streets. Degree 0/1 are impossible in a
/// valid partition.
fn node_vertices(unit_edges: &[StreetUnitEdge]) -> Vec<NodeVertex> {
    let degrees = vertex_degrees(unit_edges);
    let mut nodes = Vec::new();
    for (row, row_degrees) in degrees.iter().enumerate() {
        for (col, degree) in row_degrees.iter().enumerate() {
            let kind = match *degree {
                3.. => NodeKind::Junction,
                2 => {
                    // Corner when the two edges are perpendicular, pass
                    // anchor when they are collinear.
                    let r = row as u32;
                    let c = col as u32;
                    let vertical = unit_edges.iter().any(|e| {
                        let ((er0, ec0), (er1, ec1)) = e.vertices;
                        er0 == er1 && (ec0 == c || ec1 == c) && (er0 == r || er1 == r)
                    });
                    let horizontal = unit_edges.iter().any(|e| {
                        let ((er0, ec0), (er1, ec1)) = e.vertices;
                        ec0 == ec1 && (er0 == r || er1 == r) && (ec0 == c || ec1 == c)
                    });
                    if vertical && horizontal {
                        NodeKind::Corner
                    } else {
                        NodeKind::Pass
                    }
                }
                _ => continue,
            };
            nodes.push(NodeVertex {
                row: row as u32,
                col: col as u32,
                kind,
            });
        }
    }
    nodes
}

/// Fills the derived `is_road`/`is_junction` flags on the grid.
///
/// - `is_road`: the cell flanks at least one street unit edge (street is the
///   negative space between blocks; flagged cells border walkable street).
/// - `is_junction`: a corner vertex of the cell hosts a graph node.
fn mark_grid_flags(
    grid: &mut WorldGrid,
    unit_edges: &[StreetUnitEdge],
    node_vertices: &[NodeVertex],
) -> GameResult<()> {
    for edge in unit_edges {
        let ((r0, c0), (r1, c1)) = edge.vertices;
        // Vertical unit edge ((r, c)..(r+1, c)): the line between columns
        // c-1 and c, spanning rows r..r+1. Flanking cells: (r, c-1), (r, c).
        if c0 == c1 && r0 != r1 {
            let col = c0;
            if col > 0 {
                grid.get_cell_mut(r0, col - 1)?.is_road = true;
            }
            if col < grid.width {
                grid.get_cell_mut(r0, col)?.is_road = true;
            }
        }
        // Horizontal unit edge ((r, c)..(r, c+1)): the line between rows
        // r-1 and r, spanning cols c..c+1. Flanking cells: (r-1, c), (r, c).
        if r0 == r1 && c0 != c1 {
            let row = r0;
            if row > 0 {
                grid.get_cell_mut(row - 1, c0)?.is_road = true;
            }
            if row < grid.height {
                grid.get_cell_mut(row, c0)?.is_road = true;
            }
        }
    }
    for node in node_vertices {
        // Any cell with this vertex as a corner.
        for r in [node.row.saturating_sub(1), node.row] {
            for c in [node.col.saturating_sub(1), node.col] {
                if r < grid.height && c < grid.width {
                    grid.get_cell_mut(r, c)?.is_junction = true;
                }
            }
        }
    }
    Ok(())
}

/// Associates every street segment with the block on each side (port of the
/// original `_associate_edges_with_blocks`).
///
/// The merged runs in [`LayoutGraph`] are made of unit edges; each unit edge
/// knows its flanking cells, and their `block_id` names the block whose mitred
/// polygon ends on this very centerline. The first unit edge of a run fixes
/// the assignment: run "left" is the plane whose normal is `(-dy, dx)` of the
/// `from -> to` direction, which is the same left the player sees walking
/// from `from` to `to`. Flanks in the scenery band yield `None`.
fn associate_edges_with_blocks(
    graph: &mut LayoutGraph,
    unit_edges: &[StreetUnitEdge],
    grid: &WorldGrid,
    node_positions: &[Vec2],
) {
    for index in 0..graph.edges.len() {
        let a = graph.nodes[graph.edges[index].from as usize].pos;
        let b = graph.nodes[graph.edges[index].to as usize].pos;
        let dir = b - a;
        if dir.length_squared() < 1e-6 {
            continue;
        }
        let dir = dir.normalize();
        let left_normal = Vec2::new(-dir.y, dir.x);
        let length = (b - a).length();
        let axis = crate::world::GRID_CELLS as usize + 1;
        let lattice = |row: u32, col: u32| node_positions[row as usize * axis + col as usize];
        for unit in unit_edges {
            // Unit geometry must follow the *perturbed* lattice: the run's
            // line does. Matching base-grid centers against perturbed lines
            // left only ~60 of 2100 edges associated. Since R3.20 every unit
            // edge is one graph edge between its own endpoints, so the unit
            // midpoint lies exactly on the edge line.
            let ((ur0, uc0), (ur1, uc1)) = unit.vertices;
            let point = (lattice(ur0, uc0) + lattice(ur1, uc1)) * 0.5;
            let along = (point - a).dot(dir);
            let perp = (point - a).dot(left_normal);
            if perp.abs() > 1e-3 || along < -1e-3 || along > length + 1e-3 {
                continue;
            }
            let Some((first, second)) = flanking_blocks(unit, grid) else {
                continue;
            };
            // Assign flanks by geometry, not by unit orientation: a merged
            // run's from→to direction is arbitrary, so "which unit cell is
            // left" cannot be derived from the unit alone. The flank block
            // centre sits on one side of the run's line; whichever side that
            // is decides left/right against this run's own normal.
            let edge = &mut graph.edges[index];
            for flank in [first, second].into_iter().flatten() {
                let center = graph.blocks[flank as usize].center();
                let is_left = (center - a).dot(left_normal) > 0.0;
                if is_left {
                    edge.left_block = edge.left_block.or(Some(flank));
                } else {
                    edge.right_block = edge.right_block.or(Some(flank));
                }
            }
        }
    }
}

/// Blocks on both sides of a unit street edge, as `(north_or_west,
/// south_or_east)`: the first flank is the cell on the `-Z`/`-X` side of the
/// unit line, the second the `+Z`/`+X` side.
fn flanking_blocks(unit: &StreetUnitEdge, grid: &WorldGrid) -> Option<(Option<u32>, Option<u32>)> {
    let ((r0, c0), (r1, c1)) = unit.vertices;
    let block = |r: Option<u32>, c: Option<u32>| -> Option<u32> {
        let (r, c) = (r?, c?);
        let id = grid.get_cell(r, c).ok()?.block_id;
        (id != SCENERY_BAND_CELL).then_some(id)
    };
    let (first, second) = if r0 == r1 {
        // Horizontal edge spanning columns c0..c1: cells above (−Z) and below
        // (+Z) of the line at row r0.
        let _ = c1;
        (
            block(r0.checked_sub(1), Some(c0)),
            block(Some(r0), Some(c0)),
        )
    } else {
        // Vertical edge spanning rows r0..r1: cells west (−X) and east (+X)
        // of the line at column c0.
        (
            block(Some(r0), c0.checked_sub(1)),
            block(Some(r0), Some(c0)),
        )
    };
    (first.is_some() || second.is_some()).then_some((first, second))
}

/// Salt mixed into the seed for the spawn draw (R3.20), so the spawn
/// junction is independent of every generation draw ("CROSS" in leetspeak).
const SPAWN_SALT: u64 = 0xC1055;

/// R3.20 spawn point (2026-11-19 user decision: 出生点随机指定一个路口点，避免
/// 落地块内): picks one tier-0 [`NodeKind::Junction`] of the graph with a
/// seed-derived RNG, so the same save seed always spawns on the same
/// crossroads — no spawn data needs to live in the save format.
///
/// Returns the eye position (world XZ) and the body yaw facing down one of
/// the junction's streets (the first street edge in graph order).
///
/// The junction choice draws from its own RNG stream, never from
/// generation's, so the layout stays bit-exact regardless of spawn.
pub fn spawn_point(graph: &LayoutGraph, seed: u64) -> (Vec2, f32) {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23) ^ SPAWN_SALT;
    let mut rng = StdRng::seed_from_u64(mixed);
    let junctions: Vec<u32> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.kind == NodeKind::Junction)
        .map(|(index, _)| index as u32)
        .collect();
    if junctions.is_empty() {
        // Degenerate graph (no streets): fall back to the map centre.
        return (Vec2::splat(crate::world::WORLD_SIZE * 0.5), 0.0);
    }
    let index = junctions[rng.random_range(0..junctions.len())];
    let pos = graph.nodes[index as usize].pos;
    // Face along the junction's first tier-0 street so the first view is down
    // a road, not into a block face.
    let yaw = graph
        .edges
        .iter()
        .filter(|edge| edge.kind != EdgeKind::Lane)
        .find_map(|edge| {
            let other = if edge.from == index {
                edge.to
            } else if edge.to == index {
                edge.from
            } else {
                return None;
            };
            let d = graph.nodes[other as usize].pos - pos;
            (d.length_squared() > 1e-6).then(|| {
                let d = d.normalize();
                // PlayerBody::forward = (-sin yaw, -cos yaw); invert it.
                (-d.x).atan2(-d.y)
            })
        })
        .unwrap_or(0.0);
    (pos, yaw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legal touching tolerance for adjacent block polygons (meters). Flush
    /// outer-band joints share a centerline; sub-centimeter penetration is
    /// float noise.
    const OVERLAP_EPSILON: f32 = 0.01;

    /// Legal seam tolerance for the enclosure wall (meters): flush joints
    /// share the centerline, so the residual gap is float noise only.
    const WALL_GAP_EPSILON: f32 = 0.05;

    /// Reference world for the shared assertions.
    fn world(seed: u64) -> (WorldGrid, LayoutGraph) {
        generate_world(seed).expect("generation succeeds")
    }

    /// Re-draws the arterial cross for `seed`: `generate_world` draws it
    /// FIRST from the plain-seeded RNG, so re-seeding and drawing once
    /// reproduces it bit-exactly.
    fn cross_of(seed: u64) -> ArterialCross {
        arterial_cross(&mut StdRng::seed_from_u64(seed))
    }

    #[test]
    fn same_seed_is_bit_exact() {
        let (grid_a, graph_a) = world(0x4845_4144);
        let (grid_b, graph_b) = world(0x4845_4144);
        assert_eq!(grid_a, grid_b, "same seed must regenerate the exact grid");
        assert_eq!(
            graph_a, graph_b,
            "same seed must regenerate the exact graph"
        );
    }

    #[test]
    fn different_seeds_produce_different_maps() {
        let (grid_a, _) = world(1);
        let (grid_b, _) = world(2);
        assert_ne!(grid_a, grid_b);
    }

    #[test]
    fn partition_is_complete_and_valid() {
        let (grid, graph) = world(7);
        assert_eq!(grid.width, GRID_CELLS);
        assert_eq!(grid.cells.len(), (GRID_CELLS * GRID_CELLS) as usize);
        // Every cell is either owned by a block or part of the scenery band.
        let mut scenery = 0;
        for (idx, cell) in grid.cells.iter().enumerate() {
            if cell.block_id == SCENERY_BAND_CELL {
                scenery += 1;
                let row = idx as u32 / grid.width;
                let col = idx as u32 % grid.width;
                let distance = row
                    .min(col)
                    .min(grid.height - 1 - row)
                    .min(grid.width - 1 - col);
                assert_eq!(distance, 0, "scenery must be the outer ring only");
            } else {
                assert!(cell.block_id < graph.blocks.len() as u32);
            }
        }
        assert_eq!(
            scenery,
            2 * (GRID_CELLS + GRID_CELLS) - 4,
            "exactly one ring of cells is scenery"
        );
        // Blocks own disjoint, contiguous cell sets.
        for block in &graph.blocks {
            assert!(!block.cells.is_empty());
            let rows: Vec<u32> = block.cells.iter().map(|c| c.0).collect();
            let cols: Vec<u32> = block.cells.iter().map(|c| c.1).collect();
            let (r0, r1) = (rows.iter().min(), rows.iter().max());
            let (c0, c1) = (cols.iter().min(), cols.iter().max());
            let area = ((r1.unwrap() - r0.unwrap() + 1) * (c1.unwrap() - c0.unwrap() + 1)) as usize;
            assert_eq!(area, block.cells.len(), "block {} is a rectangle", block.id);
        }
    }

    #[test]
    fn streets_have_no_dead_ends() {
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (grid, graph) = world(seed);
            let unit_edges = street_unit_edges(&grid, &graph.blocks, cross_of(seed));
            let degrees = vertex_degrees(&unit_edges);
            for (r, row) in degrees.iter().enumerate() {
                for (c, degree) in row.iter().enumerate() {
                    assert_ne!(
                        *degree, 1,
                        "dead end at lattice vertex ({r}, {c}), seed {seed}"
                    );
                }
            }
        }
    }

    #[test]
    fn outer_band_encloses_the_world() {
        let (grid, graph) = world(7);
        let width = grid.width;
        let height = grid.height;
        // The scenery ring is not block territory: no block may own a cell on
        // the map border.
        for row in 0..height {
            for col in 0..width {
                let on_border = row.min(col).min(height - 1 - row).min(width - 1 - col) == 0;
                let cell = grid.get_cell(row, col).expect("in bounds");
                if on_border {
                    assert_eq!(
                        cell.block_id, SCENERY_BAND_CELL,
                        "border cell ({row}, {col}) must be scenery"
                    );
                } else {
                    assert_ne!(
                        cell.block_id, SCENERY_BAND_CELL,
                        "interior cell ({row}, {col}) must belong to a block"
                    );
                    assert!(cell.block_id < graph.blocks.len() as u32);
                }
            }
        }
        // Streets never touch the map border: every border lattice vertex
        // has street degree 0 (the ring road runs one cell in).
        let unit_edges = street_unit_edges(&grid, &graph.blocks, cross_of(7));
        let degrees = vertex_degrees(&unit_edges);
        let n = GRID_CELLS as usize;
        for (c, col) in degrees[0].iter().enumerate().take(n + 1) {
            assert_eq!(*col, 0, "street touches north border at col {c}");
            assert_eq!(degrees[n][c], 0, "street touches south border at col {c}");
        }
        for (r, row) in degrees.iter().enumerate().take(n + 1) {
            assert_eq!(row[0], 0, "street touches west border at row {r}");
            assert_eq!(row[n], 0, "street touches east border at row {r}");
        }
    }

    /// The enclosure band must be a seamless wall: neighboring outer blocks
    /// that share a side may only touch (flush joint), never leave a gap the
    /// player could walk through — and never interpenetrate.
    #[test]
    fn outer_band_wall_has_no_gaps() {
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (_, graph) = world(seed);
            for i in 0..graph.blocks.len() {
                for j in (i + 1)..graph.blocks.len() {
                    let (a, b) = (&graph.blocks[i], &graph.blocks[j]);
                    // Only outer blocks that share a side matter here.
                    if !(a.is_outer && b.is_outer) || !side_adjacent(a, b) {
                        continue;
                    }
                    let depth = crate::world::geometry::convex_polygon_overlap_depth(
                        &a.polygon, &b.polygon,
                    );
                    let gap = polygon_gap(&a.polygon, &b.polygon);
                    assert!(
                        depth <= OVERLAP_EPSILON,
                        "outer blocks {} and {} interpenetrate by {depth:.3} m (seed {seed})",
                        a.id,
                        b.id
                    );
                    assert!(
                        gap <= WALL_GAP_EPSILON,
                        "outer blocks {} and {} leave a {gap:.3} m gap in the wall (seed {seed})",
                        a.id,
                        b.id
                    );
                }
            }
        }
    }

    /// Side-adjacent = shares a full cell side (not just a corner).
    fn side_adjacent(a: &LayoutBlock, b: &LayoutBlock) -> bool {
        let owned: std::collections::HashSet<(u32, u32)> = a.cells.iter().copied().collect();
        b.cells.iter().any(|&(r, c)| {
            [
                (r.wrapping_sub(1), c),
                (r + 1, c),
                (r, c.wrapping_sub(1)),
                (r, c + 1),
            ]
            .iter()
            .any(|n| owned.contains(n))
        })
    }

    /// Minimum vertex-to-edge distance between two touching polygons (m).
    /// `0` when they share a vertex or edge point.
    fn polygon_gap(a: &[Vec2], b: &[Vec2]) -> f32 {
        fn point_segment_distance(p: Vec2, a: Vec2, b: Vec2) -> f32 {
            let ab = b - a;
            let len2 = ab.length_squared();
            if len2 < f32::EPSILON {
                return p.distance(a);
            }
            let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
            p.distance(a + ab * t)
        }
        let mut gap = f32::INFINITY;
        for (poly, other) in [(a, b), (b, a)] {
            for p in poly {
                for i in 0..other.len() {
                    let q = other[i];
                    let r = other[(i + 1) % other.len()];
                    gap = gap.min(point_segment_distance(*p, q, r));
                }
            }
        }
        if gap.is_finite() { gap } else { 0.0 }
    }

    /// R2.9: the grid really is perturbed — interior nodes move, border nodes
    /// stay exactly on the map edge, and the same seed reproduces them.
    /// ADR-0009: the arterial cross's two lattice lines are pinned like the
    /// ring, so the 10-15 m main road runs dead straight.
    #[test]
    fn lattice_nodes_are_perturbed_but_borders_are_pinned() {
        let cross = cross_of(7);
        let (grid, _) = world(7);
        let axis = GRID_CELLS + 1;
        assert_eq!(grid.node_positions.len(), (axis * axis) as usize);

        let mut moved = 0;
        let mut pinned_arterial = 0;
        for row in 0..axis {
            for col in 0..axis {
                let p = grid.node(row, col).expect("node in range");
                let base = Vec2::new(col as f32 * CELL_SIZE, row as f32 * CELL_SIZE);
                let on_border = row == 0 || col == 0 || row == axis - 1 || col == axis - 1;
                let on_ring_line = row == 1 || col == 1 || row == axis - 2 || col == axis - 2;
                let on_arterial = col == cross.col || row == cross.row;
                if on_border || on_ring_line {
                    assert!(
                        (p - base).length() < 1e-3,
                        "pinned node ({row}, {col}) moved {:?}",
                        p - base
                    );
                } else {
                    assert!(
                        (p - base).length() <= PERTURB_MAX * 2.0f32.sqrt() + 1e-3,
                        "node ({row}, {col}) moved too far: {:?}",
                        p - base
                    );
                    if (p - base).length() > 1e-3 {
                        moved += 1;
                    }
                }
                if on_arterial {
                    assert!(
                        (p - base).length() < 1e-3,
                        "arterial node ({row}, {col}) moved {:?}",
                        p - base
                    );
                    pinned_arterial += 1;
                }
            }
        }
        // Interior excludes the border AND the pinned ring lines.
        let interior = (axis - 4) * (axis - 4);
        assert!(
            moved as f32 / interior as f32 > 0.8,
            "expected most interior nodes to move, only {moved}/{interior} did"
        );
        // ADR-0009: the cross really pins a substantial number of nodes
        // (both lines minus the border/ring nodes they already were).
        assert!(
            pinned_arterial >= 2 * (GRID_CELLS as usize - 3),
            "arterial lines pinned only {pinned_arterial} nodes"
        );

        // Determinism: the node lattice is part of the bit-exact contract.
        let (grid_again, _) = world(7);
        assert_eq!(grid.node_positions, grid_again.node_positions);
    }

    #[test]
    fn inset_polygons_never_overlap() {
        // OVERLAPS=0: every pair of block polygons that could touch must not
        // intersect (ADR-0004 geometric guarantee, kept as a unit test).
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (_, graph) = world(seed);
            for i in 0..graph.blocks.len() {
                for j in (i + 1)..graph.blocks.len() {
                    let (a, b) = (&graph.blocks[i], &graph.blocks[j]);
                    if !blocks_adjacent(a, b) {
                        continue;
                    }
                    // Genuine interpenetration (a vertex inside the other
                    // polygon). Gate on that rather than SAT depth: with
                    // perturbation, two neighbouring edges can run within
                    // millimeters of each other while each block stays on its
                    // own side of the street, which SAT reports as overlap.
                    assert!(
                        !crate::world::geometry::convex_polygons_interpenetrate(
                            &a.polygon, &b.polygon
                        ),
                        "blocks {} and {} interpenetrate (seed {seed})",
                        a.id,
                        b.id
                    );
                }
            }
        }
    }

    /// True if the two blocks own adjacent cells (they could touch across a
    /// street or, for band blocks, sit flush together).
    fn blocks_adjacent(a: &LayoutBlock, b: &LayoutBlock) -> bool {
        let owned: std::collections::HashSet<(u32, u32)> = a.cells.iter().copied().collect();
        for &(r, c) in &b.cells {
            for n in [
                (r.wrapping_sub(1), c),
                (r + 1, c),
                (r, c.wrapping_sub(1)),
                (r, c + 1),
            ] {
                if owned.contains(&n) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn street_widths_stay_in_range() {
        let (_, graph) = world(7);
        for block in &graph.blocks {
            let (min, max) = match block.kind {
                BlockType::UrbanVillage => (VILLAGE_STREET_WIDTH_MIN, VILLAGE_STREET_WIDTH_MAX),
                _ => (STREET_WIDTH_MIN, STREET_WIDTH_MAX),
            };
            assert!(
                (min..=max).contains(&block.street_width),
                "block {} width {} out of {:?} range",
                block.id,
                block.street_width,
                block.kind
            );
        }
        // Edge width is the average of the flanking blocks, so it can be as
        // narrow as two adjacent 2.0 m village lanes (2.0 m) and as wide as
        // two adjacent 10 m blocks (10.0 m). ADR-0009: the arterial cross
        // carries its own drawn width (10-15 m) instead of the average.
        for edge in &graph.edges {
            match edge.kind {
                EdgeKind::Arterial => assert!(
                    (ARTERIAL_WIDTH_MIN..=ARTERIAL_WIDTH_MAX).contains(&edge.width),
                    "arterial edge width {} out of {:?} range",
                    edge.width,
                    (ARTERIAL_WIDTH_MIN, ARTERIAL_WIDTH_MAX)
                ),
                _ => assert!(
                    (VILLAGE_STREET_WIDTH_MIN..=STREET_WIDTH_MAX).contains(&edge.width),
                    "street edge width {} out of range",
                    edge.width
                ),
            }
        }
    }

    /// ADR-0009: exactly one vertical and one horizontal arterial, carrying
    /// the drawn widths along their whole run, meeting the ring in T
    /// junctions at both ends; no block spans the cross.
    #[test]
    fn arterial_cross_splits_the_map_into_four_quadrants() {
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (grid, graph) = world(seed);
            let cross = cross_of(seed);
            let unit_edges = street_unit_edges(&grid, &graph.blocks, cross);
            let axis = GRID_CELLS;

            // Every unit edge on the two arterial lines is arterial and
            // carries the cross's width; there are exactly `axis - 2` of
            // them per line (ring line to ring line).
            let vertical: Vec<_> = unit_edges
                .iter()
                .filter(|e| e.vertices.0.1 == e.vertices.1.1 && e.vertices.0.1 == cross.col)
                .collect();
            let horizontal: Vec<_> = unit_edges
                .iter()
                .filter(|e| e.vertices.0.0 == e.vertices.1.0 && e.vertices.0.0 == cross.row)
                .collect();
            assert_eq!(vertical.len(), (axis - 2) as usize, "seed {seed}");
            assert_eq!(horizontal.len(), (axis - 2) as usize, "seed {seed}");
            for e in vertical.iter().chain(horizontal.iter()) {
                assert_eq!(e.kind, EdgeKind::Arterial, "seed {seed}");
            }
            for e in &vertical {
                assert_eq!(e.width, cross.width_v, "seed {seed}");
            }
            for e in &horizontal {
                assert_eq!(e.width, cross.width_h, "seed {seed}");
            }

            // The graph's arterial edges form one run per axis, ring to ring:
            // every arterial edge lies on its cross line, runs parallel to it
            // (never across), and carries the drawn width. (The crossing
            // vertex hosts both runs, so orientation — not the from-node —
            // tells the runs apart.)
            let arterial: Vec<_> = graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Arterial)
                .collect();
            assert!(!arterial.is_empty(), "seed {seed}: no arterial edges");
            let mut verticals = 0;
            let mut horizontals = 0;
            for e in &arterial {
                let a = graph.nodes[e.from as usize].pos;
                let b = graph.nodes[e.to as usize].pos;
                if (a.x - b.x).abs() < 1e-3 {
                    assert!(
                        (a.x - cross.col as f32 * CELL_SIZE).abs() < 1e-3,
                        "seed {seed}: vertical arterial off its line"
                    );
                    assert!(
                        (e.width - cross.width_v).abs() < 1e-2,
                        "seed {seed}: vertical arterial width {} != {}",
                        e.width,
                        cross.width_v
                    );
                    verticals += 1;
                } else {
                    assert!(
                        (a.y - cross.row as f32 * CELL_SIZE).abs() < 1e-3,
                        "seed {seed}: horizontal arterial off its line"
                    );
                    assert!(
                        (e.width - cross.width_h).abs() < 1e-2,
                        "seed {seed}: horizontal arterial width {} != {}",
                        e.width,
                        cross.width_h
                    );
                    horizontals += 1;
                }
            }
            assert!(
                verticals > 0 && horizontals > 0,
                "seed {seed}: both runs must exist"
            );

            // No block spans the cross: every block's cells sit strictly on
            // one side of each arterial line.
            for block in &graph.blocks {
                let min_col = block.cells.iter().map(|c| c.1).min().unwrap();
                let max_col = block.cells.iter().map(|c| c.1).max().unwrap();
                let min_row = block.cells.iter().map(|c| c.0).min().unwrap();
                let max_row = block.cells.iter().map(|c| c.0).max().unwrap();
                assert!(
                    max_col < cross.col || min_col >= cross.col,
                    "seed {seed}: block {} spans the vertical arterial",
                    block.id
                );
                assert!(
                    max_row < cross.row || min_row >= cross.row,
                    "seed {seed}: block {} spans the horizontal arterial",
                    block.id
                );
            }

            // Blocks flanking the arterial retreat by the arterial half width
            // on that side. The mitred corner where an arterial edge meets a
            // side street sits at the AVERAGE of the two half widths from the
            // centreline (inset_polygon_clip_with_insets vertex rule), so the
            // guaranteed floor for ANY vertex is (arterial half + narrowest
            // village half) / 2; mid-edge arterial vertices sit at the full
            // arterial half.
            for block in &graph.blocks {
                let faces_v = block.cells.iter().any(|&(_, c)| c + 1 == cross.col)
                    || block.cells.iter().any(|&(_, c)| c == cross.col);
                if !faces_v {
                    continue;
                }
                let half = cross.width_v * 0.5;
                let floor = (half + VILLAGE_STREET_WIDTH_MIN * 0.5) * 0.5;
                for p in &block.polygon {
                    let dist = (p.x - cross.col as f32 * CELL_SIZE).abs();
                    assert!(
                        dist >= floor - 0.05,
                        "seed {seed}: block {} vertex {} m from the vertical arterial (floor {floor})",
                        block.id,
                        dist
                    );
                }
            }
        }
    }

    #[test]
    fn block_types_match_the_merge_table() {
        let (_, graph) = world(7);
        for block in &graph.blocks {
            let span = block.cells.len();
            match block.kind {
                BlockType::Plaza => {
                    assert_eq!(span, 1, "plaza blocks merge 1x1");
                }
                // R3.20: old city grows 1-2 x 1-2, then chains extensions —
                // a district can span most of a quadrant, so the bound is
                // the quadrant cell budget rather than the draw table.
                BlockType::OldCity => {
                    assert!(
                        (1..=48).contains(&span),
                        "old city district grew to {span} cells"
                    );
                }
                // R3.20 (2026-11-19 user rule): 城中村不能单块独立 — a village
                // district is never an isolated 1x1 block.
                BlockType::UrbanVillage => {
                    assert!(span >= 2, "village block {} has only {span} cell", block.id);
                }
                BlockType::Forest => {
                    // R3.20b: the draw is 2-3 per side (4-9 cells), but chain
                    // extensions grow a district further — bound by the same
                    // quadrant cell budget as old city, not by the draw table.
                    assert!(
                        (1..=48).contains(&span),
                        "forest district grew to {span} cells"
                    );
                }
                BlockType::Apartment => {
                    // Interior: 2x2/3x3 (random). Band cells are forced 1x1
                    // (the enclosure band is one cell wide).
                    assert!(
                        block.is_outer || (4..=9).contains(&span),
                        "apartment merges 2x2/3x3, got {span}"
                    );
                }
                BlockType::Factory => {
                    // 4x4/5x5 (random); clamped near the opposite border can
                    // yield non-square merges (e.g. 3x4).
                    assert!(
                        (12..=25).contains(&span),
                        "factory merges 4x4/5x5, got {span}"
                    );
                }
            }
        }
        // The world contains a realistic mix (老城区多数). R3.20b/c: the
        // factory and apartment reservations guarantee both kinds on every
        // map (the fill roll can still chain extra apartments). Forest is
        // NOT guaranteed — its border band (FOREST_EDGE_MAX_DISTANCE_CELLS)
        // competes with the reservations, and 1 of 8 measured seeds lost it;
        // a third reservation would freeze the quadrant skeleton.
        let kinds: Vec<BlockType> = graph.blocks.iter().map(|b| b.kind).collect();
        assert!(
            kinds.contains(&BlockType::Apartment),
            "no apartment district"
        );
        assert!(kinds.contains(&BlockType::Plaza));
        assert!(kinds.contains(&BlockType::Factory), "no factory district");
        let factory = kinds.iter().filter(|k| **k == BlockType::Factory).count();
        assert_eq!(factory, 1, "one reserved factory district, got {factory}");
    }

    /// R3.20 (2026-11-19 user decision: 同类地块相邻时内部道路降级): a seam
    /// between two same-kind village/old-city/forest districts is a narrow
    /// lane, not the street average the old code put there. Extension
    /// removes most seams outright; the ones the RNG declined are downgraded
    /// so the texture still reads as one neighbourhood.
    #[test]
    fn same_kind_district_seams_are_narrow_lanes() {
        for seed in [1u64, 7, 0x4845_4144] {
            let (grid, graph) = world(seed);
            let unit_edges = street_unit_edges(&grid, &graph.blocks, cross_of(seed));
            for edge in &unit_edges {
                // The arterial carries its own width regardless of flanks.
                if edge.kind != EdgeKind::Street {
                    continue;
                }
                let ((r0, c0), (_, c1)) = edge.vertices;
                // Recover the flanking cells from the edge geometry.
                let (west, east) = if c0 == c1 {
                    ((r0, c0 - 1), (r0, c0))
                } else {
                    ((r0 - 1, c0), (r0, c0))
                };
                let block = |(r, c): (u32, u32)| {
                    grid.get_cell(r, c)
                        .ok()
                        .map(|cell| cell.block_id)
                        .filter(|id| *id != SCENERY_BAND_CELL)
                };
                let (Some(a), Some(b)) = (block(west), block(east)) else {
                    continue;
                };
                let (ba, bb) = (&graph.blocks[a as usize], &graph.blocks[b as usize]);
                if ba.kind != bb.kind {
                    continue;
                }
                let expected = match ba.kind {
                    BlockType::UrbanVillage => ba.street_width.min(bb.street_width),
                    BlockType::OldCity => OLD_CITY_SEAM_WIDTH,
                    BlockType::Forest => FOREST_PATH_WIDTH,
                    _ => continue,
                };
                assert!(
                    (edge.width - expected).abs() < 1e-3,
                    "seed {seed}: same-kind {:?} seam is {} m wide, expected {expected}",
                    ba.kind,
                    edge.width
                );
            }
        }
    }

    /// R3.20: same-kind districts really do chain into contiguous runs — for
    /// village and forest cells, most same-kind neighbours are inside the
    /// same block (the extension pass), not across a street.
    #[test]
    fn same_kind_cells_coalesce_into_districts() {
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (grid, graph) = world(seed);
            for kind in [BlockType::UrbanVillage, BlockType::Forest] {
                let mut same_block = 0usize;
                let mut same_kind = 0usize;
                for row in 0..grid.height {
                    for col in 0..grid.width {
                        let id = grid.get_cell(row, col).expect("in bounds").block_id;
                        if id == SCENERY_BAND_CELL || graph.blocks[id as usize].kind != kind {
                            continue;
                        }
                        for (nr, nc) in [(row, col + 1), (row + 1, col)] {
                            let other = grid
                                .get_cell(nr, nc)
                                .ok()
                                .map(|cell| cell.block_id)
                                .unwrap_or(SCENERY_BAND_CELL);
                            if other == SCENERY_BAND_CELL {
                                continue;
                            }
                            let other_kind = graph.blocks[other as usize].kind;
                            if other_kind != kind {
                                continue;
                            }
                            same_kind += 1;
                            if other == id {
                                same_block += 1;
                            }
                        }
                    }
                }
                if same_kind == 0 {
                    continue;
                }
                let share = same_block as f32 / same_kind as f32;
                assert!(
                    share >= 0.5,
                    "seed {seed}: only {same_block}/{same_kind} same-kind {:?} adjacencies are internal (share {share:.2})",
                    kind
                );
            }
        }
    }

    /// R3.20 spawn: the seed picks one junction, deterministically, on open
    /// street — never inside a block polygon.
    #[test]
    fn spawn_point_is_a_deterministic_street_junction() {
        for seed in [1u64, 7, 0x4845_4144, 123_456_789] {
            let (_, graph) = world(seed);
            let (pos, yaw) = spawn_point(&graph, seed);
            let (pos_again, yaw_again) = spawn_point(&graph, seed);
            assert_eq!((pos, yaw), (pos_again, yaw_again), "seed {seed}");
            assert!(yaw.is_finite());
            // The spawn sits on a Junction node of the graph.
            let junction = graph.nodes.iter().enumerate().find(|(_, node)| {
                node.kind == NodeKind::Junction && (node.pos - pos).length() < 1e-3
            });
            assert!(
                junction.is_some(),
                "seed {seed}: spawn {pos:?} is not a junction node"
            );
            // And not inside any block: the walker lands on open street.
            for block in &graph.blocks {
                assert!(
                    !crate::world::geometry::point_in_convex_inclusive(pos, &block.polygon),
                    "seed {seed}: spawn {pos:?} inside block {}",
                    block.id
                );
            }
        }
    }

    #[test]
    fn road_flags_mark_street_adjacent_cells() {
        let (grid, graph) = world(7);
        // Every cell adjacent to a street must be flagged; band cells and
        // merged block interiors are not.
        let unit_edges = street_unit_edges(&grid, &graph.blocks, cross_of(7));
        let mut expected = vec![false; grid.cells.len()];
        for edge in &unit_edges {
            let ((r0, c0), (r1, c1)) = edge.vertices;
            if c0 == c1 && r0 != r1 {
                // Vertical unit edge: flanking cells to the left/right.
                if c0 > 0 {
                    expected[(r0 * grid.width + c0 - 1) as usize] = true;
                }
                if c0 < grid.width {
                    expected[(r0 * grid.width + c0) as usize] = true;
                }
            } else {
                // Horizontal unit edge: flanking cells above/below.
                if r0 > 0 {
                    expected[((r0 - 1) * grid.width + c0) as usize] = true;
                }
                if r0 < grid.height {
                    expected[(r0 * grid.width + c0) as usize] = true;
                }
            }
            let _ = (r1, c1);
        }
        for (idx, expected_road) in expected.iter().enumerate() {
            assert_eq!(
                grid.cells[idx].is_road, *expected_road,
                "is_road mismatch at cell {idx}"
            );
        }
        let roads = grid.cells.iter().filter(|c| c.is_road).count();
        assert!(roads > 0, "world must have streets");
        let junctions = graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Junction)
            .count();
        assert!(junctions > 0, "street network must have junctions");
    }
}
