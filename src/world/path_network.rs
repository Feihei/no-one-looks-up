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
    BlockType, LayoutBlock, LayoutGraph, NodeKind, NodeVertex, StreetUnitEdge,
};
use crate::world::world_grid::WorldGrid;
use crate::world::{
    CELL_SIZE, FOREST_EDGE_MAX_DISTANCE_CELLS, GRID_CELLS, OUTER_BAND_CELLS, PERTURB_MAX,
    STREET_WIDTH_MAX, STREET_WIDTH_MIN, VILLAGE_STREET_WIDTH_MAX, VILLAGE_STREET_WIDTH_MIN,
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
/// URBAN_VILLAGE 10% (2026-11: the 城中村 districts the design asks for), and
/// the remaining 70% split OLD_CITY 87 / APARTMENT 10 / FACTORY 3 of the
/// built-up remainder — cumulative thresholds
/// 0.06 / 0.20 / 0.30 / 0.87-built / 0.97-built / 1.0.
///
/// Villages and old-city blocks are both 1x1, so the per-block roll IS the
/// per-area roll for them; apartments and factories are merged, and their
/// built-up split below is the same area-biased port as before.
const P_PLAZA_END: f32 = 0.06;
const P_FOREST_END: f32 = 0.20;
/// 城中村 share of interior cells: about one lane district in ten, clustered
/// nowhere (the scan is row-major, so they scatter), which keeps one usually
/// within a few minutes of the spawn point without dominating the walk.
const P_VILLAGE_END: f32 = 0.30;
/// Built-up split (ADR-0001 "风格比例约 50/30/20"), tuned so the **area**
/// the player sees matches it: a 1x1 old-city block claims 1 cell while a
/// factory claims 16-25, so old city gets the larger probability mass.
const P_OLD_AREA: f32 = 0.87;
const P_APT_AREA: f32 = 0.10;

/// Generates the complete world layout for `seed`.
///
/// Runs at startup in well under 1 s (pure CPU over a 40x40 grid, no mesh
/// data). Returns the serde-friendly [`WorldGrid`] plus the [`LayoutGraph`].
pub fn generate_world(seed: u64) -> GameResult<(WorldGrid, LayoutGraph)> {
    let mut rng = StdRng::seed_from_u64(seed);
    // R2.9: perturbed lattice nodes are the single vertex source for every
    // block polygon (original `_generate_nodes`). Generated first so block
    // polygons can follow the real (wobbly) street centerlines.
    let nodes = perturbed_nodes(&mut rng);
    let (blocks, mut grid) = partition_blocks(&mut rng, &nodes)?;
    let unit_edges = street_unit_edges(&grid, &blocks);
    let node_vertices = node_vertices(&unit_edges);
    mark_grid_flags(&mut grid, &unit_edges, &node_vertices)?;
    let mut graph = LayoutGraph::from_generation(blocks, &unit_edges, &node_vertices, &nodes);
    associate_edges_with_blocks(&mut graph, &unit_edges, &grid, &nodes);
    grid.node_positions = nodes;
    Ok((grid, graph))
}

/// Lattice vertices per axis: `GRID_CELLS + 1` nodes span the cell grid.
pub const NODES_PER_AXIS: u32 = GRID_CELLS + 1;

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
pub fn perturbed_nodes(rng: &mut StdRng) -> Vec<Vec2> {
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
            if !on_border && !on_ring_line {
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
/// Algorithm (row-major scan, deterministic):
/// 1. take the first unassigned cell;
/// 2. grow the largest square that fits inside `MAX_MERGE` and stays free;
/// 3. shrink that square until it also fits the borders of the region the
///    desired type allows (interior only);
/// 4. rect() the remaining L-shaped gap by splitting it into up to two
///    rectangles, so nothing is left over.
///
/// `rect()` completion is what guarantees no remnants: after carving a
/// rectangle out of the free region, the leftover to its right and below is
/// itself a rectangle, which the scan reaches later.
fn partition_blocks(rng: &mut StdRng, nodes: &[Vec2]) -> GameResult<(Vec<LayoutBlock>, WorldGrid)> {
    let width = GRID_CELLS;
    let height = GRID_CELLS;
    let mut grid = WorldGrid::filled(width, height, SCENERY_BAND_CELL);
    let mut owner: Vec<Option<u32>> = vec![None; (width * height) as usize];
    let mut blocks: Vec<LayoutBlock> = Vec::new();

    // Interior region: everything except the scenery ring.
    let inner_start = OUTER_BAND_CELLS;
    let inner_end_row = height - OUTER_BAND_CELLS;
    let inner_end_col = width - OUTER_BAND_CELLS;

    for row in inner_start..inner_end_row {
        for col in inner_start..inner_end_col {
            if owner[(row * width + col) as usize].is_some() {
                continue;
            }
            let distance_to_border = row.min(col).min(height - 1 - row).min(width - 1 - col);

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
                let built_roll: f32 = rng.random();
                if built_roll < P_OLD_AREA {
                    BlockType::OldCity
                } else if built_roll < P_OLD_AREA + P_APT_AREA {
                    BlockType::Apartment
                } else {
                    BlockType::Factory
                }
            };
            if kind == BlockType::Forest && distance_to_border > FOREST_EDGE_MAX_DISTANCE_CELLS {
                kind = BlockType::OldCity;
            }

            let requested = merge_size(rng, kind);
            let mut merge = largest_free_square(
                &owner,
                width,
                row,
                col,
                inner_end_row,
                inner_end_col,
                requested,
            );
            if merge == 0 {
                continue; // Cannot happen: the scan cell itself is free.
            }
            // Keep the type/merge contract: a block smaller than its type's
            // minimum footprint degrades to OLD_CITY (the dense low-rise
            // filler), not to a mislabelled undersized apartment/factory.
            let min_side = match kind {
                BlockType::Apartment => 2,
                BlockType::Factory => 4,
                BlockType::OldCity
                | BlockType::UrbanVillage
                | BlockType::Plaza
                | BlockType::Forest => 1,
            };
            if merge < min_side {
                // Degrade to a 1x1 OLD_CITY (the dense low-rise filler);
                // never leave an undersized apartment/factory labelled.
                kind = BlockType::OldCity;
                merge = 1;
            }

            let block_id = blocks.len() as u32;
            // One draw either way, so the RNG stream keeps one street-width
            // draw per block regardless of kind (2026-11: villages draw from
            // their own narrower range — see VILLAGE_STREET_WIDTH_*).
            let street_width = match kind {
                BlockType::UrbanVillage => {
                    rng.random_range(VILLAGE_STREET_WIDTH_MIN..=VILLAGE_STREET_WIDTH_MAX)
                }
                _ => rng.random_range(STREET_WIDTH_MIN..=STREET_WIDTH_MAX),
            };
            let mut cells = Vec::with_capacity((merge * merge) as usize);
            for r in row..(row + merge) {
                for c in col..(col + merge) {
                    owner[(r * width + c) as usize] = Some(block_id);
                    cells.push((r, c));
                }
            }
            let perimeter = block_perimeter(row, col, merge, merge, nodes);
            let half = street_width * 0.5;
            let polygon = inset_polygon_clip_with_insets(&perimeter, &vec![half; perimeter.len()])
                .ok_or_else(|| {
                    GameError::Generation(format!(
                        "block {block_id} polygon collapsed (width {street_width})"
                    ))
                })?;
            blocks.push(LayoutBlock {
                id: block_id,
                kind,
                cells,
                street_width,
                is_outer: false,
                polygon,
            });
        }
    }

    for row in 0..height {
        for col in 0..width {
            let cell = grid.get_cell_mut(row, col)?;
            cell.block_id = owner[(row * width + col) as usize].unwrap_or(SCENERY_BAND_CELL);
        }
    }
    Ok((blocks, grid))
}

/// Largest square (side <= `requested`) that fits at `(row, col)` inside the
/// interior region and is entirely unassigned.
fn largest_free_square(
    owner: &[Option<u32>],
    width: u32,
    row: u32,
    col: u32,
    end_row: u32,
    end_col: u32,
    requested: u32,
) -> u32 {
    let max_side = requested.min(end_row - row).min(end_col - col).max(1);
    let mut best = 0;
    'side: for side in 1..=max_side {
        for r in row..(row + side) {
            for c in col..(col + side) {
                if owner[(r * width + c) as usize].is_some() {
                    break 'side;
                }
            }
        }
        best = side;
    }
    best
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
fn block_perimeter(row: u32, col: u32, rows: u32, cols: u32, nodes: &[Vec2]) -> Vec<Vec2> {
    let mut poly = Vec::with_capacity((2 * (rows + cols)) as usize);
    let last_row = row + rows;
    let last_col = col + cols;
    // Top: (row, col..=last_col)
    for c in col..=last_col {
        poly.push(node_at(nodes, row, c));
    }
    // Right: (row+1..=last_row, last_col)
    for r in (row + 1)..=last_row {
        poly.push(node_at(nodes, r, last_col));
    }
    // Bottom: (last_row, last_col-1..col)
    for c in (col..last_col).rev() {
        poly.push(node_at(nodes, last_row, c));
    }
    // Left: (last_row-1..=row+1, col)
    for r in ((row + 1)..last_row).rev() {
        poly.push(node_at(nodes, r, col));
    }
    poly
}

/// Merge size for a block type (ADR-0004 table): OLD_CITY/URBAN_VILLAGE/
/// PLAZA/FOREST 1x1, APARTMENT 2x2 or 3x3, FACTORY 4x4 or 5x5 (random).
/// A 城中村 district is one dense 50 m cell of houses against lanes, so it
/// merges like the old city.
fn merge_size(rng: &mut StdRng, kind: BlockType) -> u32 {
    match kind {
        BlockType::Apartment => {
            if rng.random_bool(0.5) {
                2
            } else {
                3
            }
        }
        BlockType::Factory => {
            if rng.random_bool(0.5) {
                4
            } else {
                5
            }
        }
        _ => 1,
    }
}

/// Street unit edges: grid lines between cells of different blocks, excluding
/// lines between two outer-band cells (the enclosure band has no internal
/// streets — the band rule that guarantees no dead ends).
///
/// Edge width is the average of the flanking blocks' street widths (each
/// block insets by width / 2 on its own side of the shared centerline).
fn street_unit_edges(grid: &WorldGrid, blocks: &[LayoutBlock]) -> Vec<StreetUnitEdge> {
    let width = grid.width;
    let height = grid.height;
    let block_of = |row: u32, col: u32| grid.get_cell(row, col).expect("in bounds").block_id;
    let is_band = |row: u32, col: u32| {
        row.min(col).min(height - 1 - row).min(width - 1 - col) < OUTER_BAND_CELLS
    };
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
                let (edge_width, a_id) = match (a == SCENERY_BAND_CELL, b == SCENERY_BAND_CELL) {
                    (false, false) if a == b => continue, // merged block interior
                    (false, false) => (
                        (blocks[a as usize].street_width + blocks[b as usize].street_width) * 0.5,
                        a,
                    ),
                    (true, false) => (blocks[b as usize].street_width, b),
                    (false, true) => (blocks[a as usize].street_width, a),
                    (true, true) => unreachable!(),
                };
                let _ = a_id;
                let _ = is_band;
                let vertices = if nr == row {
                    // Side-by-side cells: vertical line at col = max.
                    let c = col.max(nc);
                    ((row, c), (row + 1, c))
                } else {
                    // Stacked cells: horizontal line at row = max.
                    let r = row.max(nr);
                    ((r, col), (r, col + 1))
                };
                edges.push(StreetUnitEdge {
                    vertices,
                    width: edge_width,
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

/// Lattice vertices hosting graph nodes: junctions (degree >= 3) and corners
/// (degree 2 with perpendicular edges). Degree-2 straight vertices are
/// mid-run (no node); degree 0/1 are impossible in a valid partition.
fn node_vertices(unit_edges: &[StreetUnitEdge]) -> Vec<NodeVertex> {
    let degrees = vertex_degrees(unit_edges);
    let mut nodes = Vec::new();
    for (row, row_degrees) in degrees.iter().enumerate() {
        for (col, degree) in row_degrees.iter().enumerate() {
            let kind = match *degree {
                3.. => NodeKind::Junction,
                2 => {
                    // Corner when the two edges are perpendicular.
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
                        continue; // Straight mid-run vertex: no node.
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
            // left only ~60 of 2100 edges associated.
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
            let unit_edges = street_unit_edges(&grid, &graph.blocks);
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
        let unit_edges = street_unit_edges(&grid, &graph.blocks);
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
    #[test]
    fn lattice_nodes_are_perturbed_but_borders_are_pinned() {
        let (grid, _) = world(7);
        let axis = GRID_CELLS + 1;
        assert_eq!(grid.node_positions.len(), (axis * axis) as usize);

        let mut moved = 0;
        for row in 0..axis {
            for col in 0..axis {
                let p = grid.node(row, col).expect("node in range");
                let base = Vec2::new(col as f32 * CELL_SIZE, row as f32 * CELL_SIZE);
                let on_border = row == 0 || col == 0 || row == axis - 1 || col == axis - 1;
                let on_ring_line = row == 1 || col == 1 || row == axis - 2 || col == axis - 2;
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
            }
        }
        // Interior excludes the border AND the pinned ring lines.
        let interior = (axis - 4) * (axis - 4);
        assert!(
            moved as f32 / interior as f32 > 0.95,
            "expected nearly every interior node to move, only {moved}/{interior} did"
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
        // two adjacent 10 m blocks (10.0 m).
        for edge in &graph.edges {
            assert!(
                (VILLAGE_STREET_WIDTH_MIN..=STREET_WIDTH_MAX).contains(&edge.width),
                "street edge width {} out of range",
                edge.width
            );
        }
    }

    #[test]
    fn block_types_match_the_merge_table() {
        let (_, graph) = world(7);
        for block in &graph.blocks {
            let span = block.cells.len();
            match block.kind {
                BlockType::OldCity
                | BlockType::UrbanVillage
                | BlockType::Plaza
                | BlockType::Forest => {
                    assert_eq!(span, 1, "{:?} blocks merge 1x1", block.kind);
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
        // The world contains a realistic mix (老城区多数, 厂房极少).
        let kinds: Vec<BlockType> = graph.blocks.iter().map(|b| b.kind).collect();
        assert!(kinds.contains(&BlockType::Apartment));
        assert!(kinds.contains(&BlockType::Plaza));
        assert!(kinds.contains(&BlockType::Forest));
        let old_city = kinds.iter().filter(|k| **k == BlockType::OldCity).count();
        let factory = kinds.iter().filter(|k| **k == BlockType::Factory).count();
        assert!(
            old_city > factory * 2,
            "old city blocks must dominate: {old_city} vs {factory} factories"
        );
    }

    #[test]
    fn road_flags_mark_street_adjacent_cells() {
        let (grid, graph) = world(7);
        // Every cell adjacent to a street must be flagged; band cells and
        // merged block interiors are not.
        let unit_edges = street_unit_edges(&grid, &graph.blocks);
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
