//! Tier-1 block subdivision (ADR-0008): the lanes inside a block.
//!
//! The tier-0 skeleton (50 m lattice, merged blocks, negative-space streets)
//! is untouched. This pass runs AFTER `associate_edges_with_blocks` and cuts
//! the interior of each built block into house rows separated by full-depth
//! lanes, then each row into flush plots ([`SubBlock`]). Every lane runs the
//! whole depth of its block and ends on the flanking tier-0 streets: each
//! end is a [`NodeKind::LaneMouth`] that splits the street edge it lands on,
//! the walking-scale T junction the ADR exists to produce. Neighbouring
//! blocks pick their own axis and offsets, so lanes never continue across a
//! street.
//!
//! Rules pinned by the ADR (and by the tests below):
//! - per-type intensity: village 13 m pitch / 2.4-3.2 m lanes, old city
//!   24 m / 3.5-5.0 m; apartment (R3.20), factory, plaza and forest are
//!   never subdivided;
//! - the main axis prefers the longer side, flipped with 30% probability so
//!   the map does not read as one-directional stripes;
//! - plots inside a row sit FLUSH against each other: the wall-to-wall feel
//!   comes from `BuildingParams.spacing`/`setback`, never from plot gaps;
//! - lanes are never broken up inside a row (the rejected prototype D: a
//!   field of disconnected dead-end stubs);
//! - lane width caps at the narrower flanking street's width so a mouth
//!   never opens wider than the street it ends on.
//!
//! Determinism: the pass draws from its OWN RNG (`subdivision_rng`), never
//! from the generation stream, so every tier-0 decision (and therefore every
//! pre-ADR-0008 street, node and block polygon) stays bit-exact. Block order
//! is the generation order; within a block the draw order is fixed:
//! axis flip, pitch jitter, lane width, then per-row plot lengths.
//!
//! Graph bookkeeping: lane edges and the second halves of split streets are
//! APPENDED after the original street edges, so every pre-existing edge
//! index (and with it every per-edge RNG stream of placement/props/overhead)
//! keeps its identity. A street edge split at a mouth keeps its index for
//! the first half. Mouth nodes are appended after all tier-0 nodes.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use bevy::math::Vec2;

use crate::world::geometry::{
    clip_half_plane, inward_normals, polygon_area, polygon_centroid, polygon_is_convex,
};
use crate::world::layout_graph::{
    BlockType, EdgeKind, LayoutBlock, LayoutEdge, LayoutGraph, LayoutNode, NodeKind, SubBlock,
    SubFrontage,
};

/// Per-type subdivision intensity (ADR-0008 parameter table).
#[derive(Clone, Copy, Debug)]
pub struct SubdivisionParams {
    /// Target lane pitch (m), centreline to centreline.
    pub pitch: f32,
    /// Random pitch spread: the block draws `pitch * [1 - spread, 1 + spread]`.
    pub pitch_spread: f32,
    /// Lane width range (m).
    pub lane_width: (f32, f32),
    /// Target plot length along a row (m); each plot draws 0.6-1.4x of it.
    pub plot_length: f32,
}

/// Subdivision parameters of a block type, or `None` for types that keep
/// their interior whole (factory halls, walkable plazas and forests — and,
/// since R3.20 2026-11-19 user decision, apartment districts: 蓝色公寓楼区
/// 不再切内部巷道, the mid-rise slabs line the block's streets directly, so
/// the blue districts read as solid estates without internal roads).
pub fn params_of(kind: BlockType) -> Option<SubdivisionParams> {
    match kind {
        BlockType::UrbanVillage => Some(SubdivisionParams {
            pitch: 13.0,
            pitch_spread: 0.23,
            lane_width: (2.4, 3.2),
            plot_length: 11.0,
        }),
        BlockType::OldCity => Some(SubdivisionParams {
            pitch: 24.0,
            pitch_spread: 0.3,
            lane_width: (3.5, 5.0),
            plot_length: 18.0,
        }),
        BlockType::Factory | BlockType::Apartment | BlockType::Plaza | BlockType::Forest => None,
    }
}

/// Smallest plot area kept (m²). Slivers at a mitred block corner carry no
/// building and only inflate the collision count, so they are dropped.
const MIN_PLOT_AREA: f32 = 8.0;

/// Shortest side of a kept plot along either axis (m). A clipped sliver
/// thinner than this cannot hold a building and produces false overlap
/// readings against its flush neighbour, so it is dropped.
const MIN_PLOT_SIDE: f32 = 2.0;

/// Shortest lane kept (m). A lane shorter than this is a stub, not a road.
const MIN_LANE_LENGTH: f32 = 6.0;

/// How close a lane mouth may come to a street node before it is dropped
/// (m): a mouth on top of a junction adds nothing and reads as a crack.
const MOUTH_NODE_CLEARANCE: f32 = 2.0;

/// Dedicated RNG of the subdivision pass. Seeded from the world seed with
/// its own salt, so the tier-0 generation stream never sees these draws and
/// a pre-ADR-0008 world stays bit-exact.
pub fn subdivision_rng(seed: u64) -> StdRng {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23) ^ 0xAD80_0008_5EED_C0DE; // "ADR-0008"
    StdRng::seed_from_u64(mixed)
}

/// One planned lane: a centerline segment inside the parent polygon plus
/// its half width. The centerline is `origin + dir * t` for `t in lo..=hi`,
/// clipped to the parent polygon.
#[derive(Clone, Copy, Debug)]
struct Lane {
    origin: Vec2,
    dir: Vec2,
    lo: f32,
    hi: f32,
    half: f32,
    /// Cross-axis offset of the centerline from the block centroid.
    offset: f32,
}

impl Lane {
    fn point(&self, t: f32) -> Vec2 {
        self.origin + self.dir * t
    }
}

/// Runs the subdivision pass over every eligible block of `graph`.
///
/// Appends lane edges (and their mouth nodes) and fills `block.children`.
/// Tier-0 nodes and street lines are never moved; a street edge hosting a
/// mouth is split collinearly, preserving its line and width exactly.
pub fn subdivide_blocks(graph: &mut LayoutGraph, seed: u64) {
    let mut rng = subdivision_rng(seed);
    for index in 0..graph.blocks.len() {
        let Some(params) = params_of(graph.blocks[index].kind) else {
            continue;
        };
        if graph.blocks[index].is_outer || graph.blocks[index].polygon.len() < 3 {
            continue;
        }
        let block = graph.blocks[index].clone();
        subdivide_one(graph, index, &block, &params, &mut rng);
    }
}

/// Subdivides one block: plans the lanes, cuts the plots, wires the graph.
fn subdivide_one(
    graph: &mut LayoutGraph,
    block_index: usize,
    block: &LayoutBlock,
    params: &SubdivisionParams,
    rng: &mut StdRng,
) {
    let polygon = &block.polygon;
    let origin = polygon_centroid(polygon);

    // Main axis: the longer bounding-box side, flipped with 30% probability
    // (ADR-0008: the flip is what keeps the map from reading as stripes).
    // Rows run ALONG the main axis; lanes cross it.
    let (min, max) = bounds_of(polygon);
    let size = max - min;
    let mut axis = if size.x >= size.y {
        Vec2::new(1.0, 0.0)
    } else {
        Vec2::new(0.0, 1.0)
    };
    if rng.random_bool(0.3) {
        axis = Vec2::new(-axis.y, axis.x);
    }
    let cross = Vec2::new(-axis.y, axis.x);

    // Lane count from the cross-axis extent. The jitter draw happens even
    // when the count floors to zero, so the stream stays aligned per block.
    let cross_extent = extent_along(polygon, origin, cross);
    let jitter: f32 = rng.random_range(1.0 - params.pitch_spread..=1.0 + params.pitch_spread);
    let pitch = params.pitch * jitter;
    let lane_count = (cross_extent / pitch).floor().max(0.0) as usize;
    let lane_width: f32 = rng.random_range(params.lane_width.0..=params.lane_width.1);
    if std::env::var_os("HU_TRACE_SUBDIV").is_some() {
        eprintln!(
            "subdiv block {} {:?} size {:?} cross_extent {:.1} pitch {:.1} lanes {}",
            block.id, block.kind, size, cross_extent, pitch, lane_count
        );
    }
    if lane_count == 0 {
        return;
    }

    // Lane offsets across the block: evenly spread with the shared jittered
    // pitch, centred so the two outer rows keep equal depth.
    let span = (lane_count - 1) as f32 * pitch;
    let offsets: Vec<f32> = (0..lane_count)
        .map(|i| -span * 0.5 + i as f32 * pitch)
        .collect();

    // Cap the lane width at the narrower flanking street so a mouth never
    // opens wider than the street it ends on (ADR-0008 risk table).
    let flank_width = flanking_street_width(graph, block);
    let half = (lane_width * 0.5).min(flank_width * 0.5);

    // Clip each lane centerline to the parent polygon.
    let mut lanes = Vec::new();
    for &offset in &offsets {
        let line_origin = origin + cross * offset;
        let (lo, hi) = chord_along(polygon, line_origin, axis);
        if hi - lo < MIN_LANE_LENGTH {
            continue;
        }
        lanes.push(Lane {
            origin: line_origin,
            dir: axis,
            lo,
            hi,
            half,
            offset,
        });
    }
    if lanes.is_empty() {
        if std::env::var_os("HU_TRACE_SUBDIV").is_some() {
            eprintln!("subdiv block {}: all lane chords filtered", block.id);
        }
        return;
    }

    // Wire the lanes into the graph FIRST: a lane that cannot reach a
    // street on both ends is dropped whole, and only surviving lanes cut
    // plots (no sealed strips behind a lane that never opened).
    let mut opened: Vec<Lane> = Vec::new();
    for lane in &lanes {
        if lane_edge(graph, block.id, lane).is_some() {
            opened.push(*lane);
        }
    }
    // Rows are cut at EVERY planned lane, whether or not that lane could be
    // wired into the street graph. A lane whose mouths both landed on existing
    // nodes contributes no edge (it would be a crossroads, not a T), but the
    // strip between plots still has to exist — otherwise the block collapsed
    // into a couple of huge slabs whenever one lane failed.
    if opened.is_empty() {
        // No lane could reach a street on both ends (both crossings landed on
        // existing nodes, or the block edge has no tier-0 street behind it).
        // The block still gets a PLOT GRID, just without lane edges: the
        // street-facing rows are placed per plot, which keeps the block from
        // reading as a hollow shell. No lane edge means no T, so the ADR's
        // T-junction guarantee is unaffected — this block simply contributes
        // no lanes.
        let fallback = plot_grid_without_lanes(block, params, origin, axis, cross, rng);
        if !fallback.is_empty() {
            graph.blocks[block_index].children = fallback;
        }
        return;
    }

    // House rows: the strips between consecutive lane EDGES plus the two
    // outer strips. Interior rows are cut into flush plots. The OUTER rows
    // (facing the tier-0 streets) are kept as ONE plot each: their street
    // face must stay a single frontage for the tier-0 pass (a plot-wide
    // slice of it would fall under MIN_FACADE and blank the street), while
    // their lane face still gets a tier-1 row and their collision still
    // opens the lane.
    let (cross_lo, cross_hi) = extent_range(polygon, origin, cross);
    let mut row_bounds: Vec<(f32, f32)> = Vec::with_capacity(lanes.len() + 1);
    let mut cursor = cross_lo;
    for lane in &lanes {
        row_bounds.push((cursor, lane.offset - lane.half));
        cursor = lane.offset + lane.half;
    }
    row_bounds.push((cursor, cross_hi));
    let last_row = row_bounds.len() - 1;

    // Cut each interior row into flush plots along the main axis. Plot cuts
    // are RANDOM TARGETS snapped to a fine lattice: two neighbouring clips
    // that share a cut then agree bit-exactly, so flush plots can never
    // disagree by a float hair (which the strict interpenetration judge
    // would read as overlap).
    let mut children: Vec<SubBlock> = Vec::new();
    for (row_index, (row_lo, row_hi)) in row_bounds.into_iter().enumerate() {
        let mut row_poly = polygon.clone();
        if row_lo.is_finite() {
            row_poly = clip_half_plane(&row_poly, cross, cross.dot(origin) + row_lo);
        }
        if row_hi.is_finite() {
            row_poly = clip_half_plane(&row_poly, -cross, -(cross.dot(origin) + row_hi));
        }
        if row_poly.len() < 3 || polygon_area(&row_poly).abs() < MIN_PLOT_AREA {
            continue;
        }
        let (axis_lo, axis_hi) = extent_range(&row_poly, origin, axis);
        if axis_hi - axis_lo < MIN_PLOT_SIDE {
            continue;
        }

        // Outer rows stay ONE plot (the street face must remain a single
        // frontage for the tier-0 pass); interior rows are cut into flush
        // plots. Plot boundaries: random lengths 0.6-1.4x the type's
        // target, flush against each other (no gap: the ADR's rule 4). The
        // last plot takes the remainder so the row is fully tiled.
        let outer = row_index == 0 || row_index == last_row;
        let mut cuts = vec![axis_lo];
        if !outer {
            let mut cursor = axis_lo;
            loop {
                let len: f32 = params.plot_length * rng.random_range(0.6..=1.4);
                // Snap the cut to a 1/64 m lattice: identical values on both
                // sides of a shared boundary, no float-hair interpenetration.
                let next = ((cursor + len) * 64.0).round() / 64.0;
                if next >= axis_hi - params.plot_length * 0.45 {
                    break;
                }
                cuts.push(next);
                cursor = next;
            }
        }
        cuts.push(axis_hi);

        for pair in cuts.windows(2) {
            let (lo, hi) = (pair[0], pair[1]);
            if hi - lo < MIN_PLOT_SIDE {
                continue;
            }
            let mut plot = row_poly.clone();
            plot = clip_half_plane(&plot, axis, axis.dot(origin) + lo);
            plot = clip_half_plane(&plot, -axis, -(axis.dot(origin) + hi));
            if plot.len() < 3 || polygon_area(&plot).abs() < MIN_PLOT_AREA {
                continue;
            }
            // Drop clipped slivers: a plot whose extent along either axis
            // collapsed below the minimum side is a corner shaving, not a
            // building plot.
            let (a0, a1) = extent_range(&plot, origin, axis);
            let (c0, c1) = extent_range(&plot, origin, cross);
            if a1 - a0 < MIN_PLOT_SIDE || c1 - c0 < MIN_PLOT_SIDE {
                continue;
            }
            // Clip back into the parent: at a strongly tilted boundary the
            // row/plot clip intersections can land a hair outside the
            // parent edge. Clipping against the parent's own half-planes
            // is exact and keeps the plot inside by construction — but only
            // for a CONVEX parent. R3.20 districts are mildly non-convex (a
            // mid-edge lattice node dents by its ±4 m wobble), and clipping
            // against a reflex edge's half-plane would carve valid interior
            // away (measured: a 3x3 village lost half its plots and grew
            // malformed slivers). Non-convex parents skip this pass: the
            // plots are already inside the parent by construction (they are
            // sequential clips OF the parent polygon), and the quantised-
            // vertex slack is covered by the containment test's epsilon.
            let parent = polygon;
            if polygon_is_convex(parent) {
                let normals = inward_normals(parent);
                let mut inside = true;
                for (i, normal) in normals.iter().enumerate() {
                    if normal.length_squared() < 1e-12 {
                        continue;
                    }
                    let pa = parent[i];
                    plot = clip_half_plane(&plot, *normal, normal.dot(pa));
                    if plot.len() < 3 {
                        inside = false;
                        break;
                    }
                }
                if !inside {
                    continue;
                }
            }
            if polygon_area(&plot).abs() < MIN_PLOT_AREA {
                continue;
            }
            // CCW winding is the SubBlock contract; normalise once here.
            if polygon_area(&plot) < 0.0 {
                plot.reverse();
            }
            // Quantise every vertex to a 1/256 m lattice (~4 mm). Two
            // flush plots clip the same shared boundary from opposite
            // sides; the raw intersections can differ by a float hair,
            // which the strict interpenetration judge reads as overlap.
            // Snapping both to the same lattice makes the shared edge
            // bit-identical. 4 mm is far below any visible or walkable
            // scale, and the parent-containment test carries a matching
            // tolerance.
            for p in &mut plot {
                *p = Vec2::new((p.x * 256.0).round() / 256.0, (p.y * 256.0).round() / 256.0);
            }
            plot.dedup();
            if plot.len() < 3 || polygon_area(&plot).abs() < MIN_PLOT_AREA {
                continue;
            }
            let frontages = plot_frontages(&plot, &opened);
            children.push(SubBlock {
                parent: block.id,
                polygon: plot,
                frontages,
            });
        }
    }
    let child_count = children.len();
    if std::env::var_os("HU_TRACE_SUBDIV").is_some() {
        eprintln!(
            "subdiv block {} done: lanes {} opened {} children {}",
            block.id,
            lanes.len(),
            opened.len(),
            child_count
        );
    }
    graph.blocks[block_index].children = children;
}

/// Cuts a block into house rows WITHOUT lane edges.
///
/// Used only when every lane of the block failed to reach a tier-0 street at
/// both ends (its face has no street behind it, or both crossings landed on
/// existing nodes). The plots are still real (`SubBlock`), so placement lines
/// each row along the plot's own street-facing edge; the block just carries no
/// tier-1 lane edges, and therefore contributes no T junctions.
///
/// The cut runs across the main axis at the mid-point plus one jitter, which
/// is enough to give the block 2-3 rows instead of a single perimeter ring.
fn plot_grid_without_lanes(
    block: &LayoutBlock,
    params: &SubdivisionParams,
    origin: Vec2,
    axis: Vec2,
    cross: Vec2,
    rng: &mut StdRng,
) -> Vec<SubBlock> {
    let polygon = &block.polygon;
    if polygon.len() < 3 {
        return Vec::new();
    }
    // House rows stack along the block's SHORTER extent (each row is a house
    // deep), so the cuts are planes perpendicular to that axis. Counting
    // along the long extent split a square block into two slabs instead of
    // rows of plots.
    let (long_lo, long_hi) = extent_range(polygon, origin, cross);
    let depth = long_hi - long_lo;
    if depth < MIN_PLOT_SIDE * 2.0 {
        return Vec::new();
    }
    let rows = ((depth / params.pitch).round().max(1.0) as usize).min(6);
    let mut bounds = Vec::with_capacity(rows + 1);
    let step = depth / rows as f32;
    for i in 0..=rows {
        bounds.push(long_lo + step * i as f32);
    }
    // Jitter the interior cuts so the rows are not perfectly even.
    for cut in bounds.iter_mut().skip(1).take(rows.saturating_sub(1)) {
        *cut += rng.random_range(-step * 0.12..=step * 0.12);
    }

    // Also cut each row along the frontage: a row of one house is the ADR's
    // plot, so a 47 m wide row must become several plots. Without this the
    // fallback produced 2 huge slabs instead of a plot grid.
    let (along_lo, along_hi) = extent_range(polygon, origin, axis);
    let along = along_hi - along_lo;
    let cols = ((along / params.plot_length).round().max(1.0) as usize).min(8);
    let col_step = along / cols as f32;
    let mut col_bounds = Vec::with_capacity(cols + 1);
    for i in 0..=cols {
        col_bounds.push(along_lo + col_step * i as f32);
    }
    for cut in col_bounds.iter_mut().skip(1).take(cols.saturating_sub(1)) {
        *cut += rng.random_range(-col_step * 0.15..=col_step * 0.15);
    }

    let mut out = Vec::new();
    for pair in bounds.windows(2) {
        let (lo, hi) = (pair[0], pair[1]);
        if hi - lo < MIN_PLOT_SIDE {
            continue;
        }
        let mut band = clip_half_plane(polygon, cross, cross.dot(origin) + lo);
        band = clip_half_plane(&band, -cross, -(cross.dot(origin) + hi));
        if band.len() < 3 || polygon_area(&band).abs() < MIN_PLOT_AREA {
            continue;
        }
        for col in col_bounds.windows(2) {
            let (clo, chi) = (col[0], col[1]);
            if chi - clo < MIN_PLOT_SIDE {
                continue;
            }
            let mut plot = clip_half_plane(&band, axis, axis.dot(origin) + clo);
            plot = clip_half_plane(&plot, -axis, -(axis.dot(origin) + chi));
            if plot.len() < 3 || polygon_area(&plot).abs() < MIN_PLOT_AREA {
                continue;
            }
            // Clip back into the parent: a tilted block's straight cut can
            // land a hair outside a mitred corner, and containment (plus the
            // collision prisms) require the plot to stay inside.
            let normals = inward_normals(polygon);
            let mut inside = true;
            for (i, normal) in normals.iter().enumerate() {
                if normal.length_squared() < 1e-12 {
                    continue;
                }
                let pa = polygon[i];
                plot = clip_half_plane(&plot, *normal, normal.dot(pa));
                if plot.len() < 3 {
                    inside = false;
                    break;
                }
            }
            if !inside || polygon_area(&plot).abs() < MIN_PLOT_AREA {
                continue;
            }
            if polygon_area(&plot) < 0.0 {
                plot.reverse();
            }
            let (a0, a1) = extent_range(&plot, origin, axis);
            let (c0, c1) = extent_range(&plot, origin, cross);
            if a1 - a0 < MIN_PLOT_SIDE || c1 - c0 < MIN_PLOT_SIDE {
                continue;
            }
            out.push(SubBlock {
                parent: block.id,
                polygon: plot,
                // No lanes, so no lane frontages; placement lines these plots
                // from their street-facing edge (against the block outline).
                frontages: Vec::new(),
            });
        }
    }
    out
}

/// Lane-facing boundaries of a plot: edges that lie on a lane's edge line.
fn plot_frontages(plot: &[Vec2], lanes: &[Lane]) -> Vec<SubFrontage> {
    let mut out = Vec::new();
    let n = plot.len();
    for i in 0..n {
        let a = plot[i];
        let b = plot[(i + 1) % n];
        let edge = b - a;
        if edge.length_squared() < 1e-6 {
            continue;
        }
        let dir = edge.normalize();
        for lane in lanes {
            // The plot edge must be parallel to the lane and sit exactly
            // one lane half-width from its centerline.
            if dir.perp_dot(lane.dir).abs() > 1e-3 {
                continue;
            }
            let normal = Vec2::new(-lane.dir.y, lane.dir.x);
            let d = normal.dot(a - lane.origin);
            if (d.abs() - lane.half).abs() > 1e-2 {
                continue;
            }
            // And it must overlap the lane's actual run, not its extension.
            let ta = lane.dir.dot(a - lane.origin);
            let tb = lane.dir.dot(b - lane.origin);
            let (lo, hi) = if ta < tb { (ta, tb) } else { (tb, ta) };
            if hi < lane.lo - 1e-2 || lo > lane.hi + 1e-2 {
                continue;
            }
            out.push(SubFrontage {
                a,
                b,
                lane_half_width: lane.half,
            });
            break;
        }
    }
    out
}

/// Width of the narrowest tier-0 street flanking the block — the streets a
/// lane's mouths can end on. R3.20: same-kind district seams are narrow
/// lanes (2-4 m), and a lane (village up to 3.2 m, old city up to 5.0 m)
/// must never open wider than the seam street it ends on. The old
/// orientation-filtered version only ever saw the wide streets, which was
/// harmless while every street was 6-10 m but let a 4.3 m lane end on a
/// 4.0 m seam. Falls back to the block's own street width when no flanking
/// edge is found.
fn flanking_street_width(graph: &LayoutGraph, block: &LayoutBlock) -> f32 {
    let mut width = block.street_width;
    for edge in &graph.edges {
        // Lanes and the arterial both flank the block: only lanes are
        // excluded (ADR-0009).
        if edge.kind == EdgeKind::Lane {
            continue;
        }
        if edge.left_block != Some(block.id) && edge.right_block != Some(block.id) {
            continue;
        }
        if edge.width < width {
            width = edge.width;
        }
    }
    width
}

/// Builds the Lane edge for one lane: finds the tier-0 street edges its
/// endpoints land on, splits them at the mouth points, and connects the two
/// mouth nodes. Returns the new edge (already appended) or `None` when a
/// mouth cannot be placed (the lane is then dropped whole).
fn lane_edge(graph: &mut LayoutGraph, block_id: u32, lane: &Lane) -> Option<LayoutEdge> {
    let a = lane.point(lane.lo);
    let b = lane.point(lane.hi);
    // ADR-0008: a lane ends at a T on a tier-0 street, both ends. A lane that
    // cannot reach a street on both ends is dropped whole (the tests pin
    // this); the fallback grid in the caller keeps such a block from ending
    // up hollow.
    let mouth_a = place_mouth(graph, block_id, a, lane.dir);
    let mouth_b = place_mouth(graph, block_id, b, lane.dir);
    if std::env::var_os("HU_TRACE_MOUTH").is_some() {
        eprintln!(
            "lane_edge block {} dir {:?} mouths ({:?}, {:?}) a {:?} b {:?}",
            block_id, lane.dir, mouth_a, mouth_b, a, b
        );
    }
    let mouth_a = mouth_a?;
    let mouth_b = mouth_b?;
    if mouth_a == mouth_b {
        return None;
    }
    let edge = LayoutEdge {
        from: mouth_a,
        to: mouth_b,
        width: lane.half * 2.0,
        left_block: Some(block_id),
        right_block: Some(block_id),
        kind: EdgeKind::Lane,
    };
    graph.edges.push(edge.clone());
    Some(edge)
}

/// Places one lane mouth: the point where the lane's centerline crosses the
/// parent block's boundary sits one half street width short of the tier-0
/// centerline. Finds the street edge whose line the lane crosses within
/// that reach, splits it at the crossing, and returns the mouth node index.
fn place_mouth(
    graph: &mut LayoutGraph,
    block_id: u32,
    endpoint: Vec2,
    lane_dir: Vec2,
) -> Option<u32> {
    let block = graph.blocks.iter().find(|b| b.id == block_id)?;
    // Reach past the polygon boundary to the street centerline, PER CANDIDATE
    // EDGE (ADR-0009): the block insets by half the FLANKING street's width on
    // each side — its own street width on ordinary sides, the arterial's half
    // width (up to 7.5 m) on the arterial sides — plus slack for the mitred
    // corners. The chord endpoint sits on the mitred boundary, whose
    // perpendicular distance to the street line exceeds that half width where
    // the polygon edge is angled. Search generously per edge, then keep the
    // CLOSEST crossing, so a lane still finds its street on a perturbed block
    // and never snaps to a street on the OPPOSITE side of its block (measured:
    // |s| up to 45 m, producing a lane that cut across the whole block).
    let reach = |edge_width: f32| edge_width * 0.5 + 3.0;
    // Where the mouth may sit ALONG the street: the lane's chord endpoints
    // are mitred inward, so the true crossing can slide a few metres along
    // the street away from the endpoint's projection. Bounded so a lane
    // never bends into a visibly diagonal stub.
    let along_slack = block.street_width + 3.0;

    let mut best: Option<(usize, f32, Vec2)> = None; // (edge index, t, point)
    for (index, edge) in graph.edges.iter().enumerate() {
        // Lanes never host mouths; streets and the arterial both do
        // (a lane may end in a T on the main road, ADR-0009).
        if edge.kind == EdgeKind::Lane {
            continue;
        }
        if edge.left_block != Some(block_id) && edge.right_block != Some(block_id) {
            continue;
        }
        let a = graph.nodes[edge.from as usize].pos;
        let b = graph.nodes[edge.to as usize].pos;
        let ab = b - a;
        let len2 = ab.length_squared();
        if len2 < 1e-9 {
            continue;
        }
        // The street must cross the lane (not run along it).
        let dir = ab / len2.sqrt();
        if dir.perp_dot(lane_dir).abs() < 0.2 {
            continue;
        }
        let Some((t, s, point)) = line_crossing(a, ab, endpoint, lane_dir) else {
            continue;
        };
        // The crossing must sit just OUTSIDE the polygon boundary along the
        // lane direction (within this edge's reach), on the outward side. The
        // endpoint's own side decides the sign: `lane_dir` points from the
        // `lo` end to the `hi` end, so at the `lo` end outward is -dir.
        // `s` is measured along +lane_dir from the endpoint; accept the
        // crossing on either outward side within the reach.
        if s.abs() > reach(edge.width) {
            continue;
        }
        // And it must stay near the lane's own line: the perpendicular
        // slide along the street is bounded so the lane never kinks.
        let slide = (point - endpoint).dot(dir);
        if slide.abs() > along_slack {
            continue;
        }
        match best {
            Some((_, _, prev)) if prev.distance(endpoint) <= point.distance(endpoint) => {}
            _ => best = Some((index, t, point)),
        }
    }
    let (edge_index, _t, point) = best?;

    // A mouth must not land on an existing node: joining it would make the
    // node degree 4 (a crossroads), and ADR-0008 — plus the tests — require a
    // lane to end in a T (degree 3). If the crossing is too close to a node,
    // try the same lane's other end; if both are, the lane is dropped and the
    // block keeps its plots via `subdivide_without_lane` below.
    let edge = &graph.edges[edge_index];
    for node in [edge.from, edge.to] {
        if graph.nodes[node as usize].pos.distance(point) < MOUTH_NODE_CLEARANCE {
            return None;
        }
    }

    // Split the street edge at the mouth: two collinear sub-edges keep the
    // exact line and width, and the mouth node joins them (degree 3 with
    // the lane = the T). The first half keeps the edge index so per-edge
    // RNG streams of the tier-0 furniture stay stable.
    let mouth = graph.nodes.len() as u32;
    graph.nodes.push(LayoutNode {
        pos: point,
        kind: NodeKind::LaneMouth,
    });
    let street = graph.edges[edge_index].clone();
    let first = LayoutEdge {
        from: street.from,
        to: mouth,
        ..street.clone()
    };
    let second = LayoutEdge {
        from: mouth,
        to: street.to,
        ..street.clone()
    };
    graph.edges[edge_index] = first;
    graph.edges.push(second);
    Some(mouth)
}

/// Crossing of the segment `a..a+ab` with the infinite line through
/// `endpoint` along `lane_dir`. Returns `(t along segment, s along lane
/// line from endpoint, point)`.
fn line_crossing(a: Vec2, ab: Vec2, endpoint: Vec2, lane_dir: Vec2) -> Option<(f32, f32, Vec2)> {
    let denom = ab.perp_dot(lane_dir);
    if denom.abs() < 1e-9 {
        return None;
    }
    let rhs = endpoint - a;
    let t = rhs.perp_dot(lane_dir) / denom;
    // A block face is made of several short street pieces (each lane mouth
    // splits one), so the lane's line can cross just past a piece's end. A
    // small tolerance lets that piece still own the mouth; the point is then
    // clamped onto the segment so it stays on the street.
    // A block face is a chain of short street pieces (each mouth splits
    // one), so a lane's line often crosses just past the piece it was tested
    // against. Accept a generous band and clamp the point ONTO the segment:
    // the mouth then sits at the piece's end, which is exactly where the
    // neighbouring piece begins, so the T is still on the street.
    if !(-0.35..=1.35).contains(&t) {
        return None;
    }
    let s = rhs.perp_dot(ab) / denom;
    let t_clamped = t.clamp(0.0, 1.0);
    Some((t_clamped, s, a + ab * t_clamped))
}

/// Bounding box of a polygon.
fn bounds_of(polygon: &[Vec2]) -> (Vec2, Vec2) {
    let mut min = Vec2::splat(f32::INFINITY);
    let mut max = Vec2::splat(f32::NEG_INFINITY);
    for p in polygon {
        min = min.min(*p);
        max = max.max(*p);
    }
    (min, max)
}

/// Full extent of the polygon along `dir` through `origin` (m).
fn extent_along(polygon: &[Vec2], origin: Vec2, dir: Vec2) -> f32 {
    let (lo, hi) = extent_range(polygon, origin, dir);
    hi - lo
}

/// Min/max projection of the polygon onto `dir` from `origin`.
fn extent_range(polygon: &[Vec2], origin: Vec2, dir: Vec2) -> (f32, f32) {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for p in polygon {
        let t = dir.dot(*p - origin);
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (lo, hi)
}

/// Chord of the polygon along the line `origin + dir * t`: the parameter
/// range where the line is inside. For a convex polygon there are exactly
/// two boundary crossings.
fn chord_along(polygon: &[Vec2], origin: Vec2, dir: Vec2) -> (f32, f32) {
    let normal = Vec2::new(-dir.y, dir.x);
    let d = normal.dot(origin);
    let mut hits: Vec<f32> = Vec::new();
    let n = polygon.len();
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let da = normal.dot(a) - d;
        let db = normal.dot(b) - d;
        if (da > 0.0 && db < 0.0) || (da < 0.0 && db > 0.0) {
            let t = da / (da - db);
            let p = a.lerp(b, t);
            hits.push(dir.dot(p - origin));
        } else if da.abs() < 1e-9 {
            hits.push(dir.dot(a - origin));
        }
    }
    if hits.len() < 2 {
        return (0.0, 0.0);
    }
    let lo = hits.iter().copied().fold(f32::INFINITY, f32::min);
    let hi = hits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::geometry::{dist_point_to_segment, point_in_polygon};
    use crate::world::path_network::generate_world;

    const SEEDS: [u64; 4] = [1, 7, 0x4845_4144, 123_456_789];

    fn world(seed: u64) -> LayoutGraph {
        generate_world(seed).expect("generation").1
    }

    #[test]
    fn params_match_the_adr_table() {
        let village = params_of(BlockType::UrbanVillage).unwrap();
        assert_eq!(village.pitch, 13.0);
        assert_eq!(village.lane_width, (2.4, 3.2));
        assert_eq!(village.plot_length, 11.0);
        let old = params_of(BlockType::OldCity).unwrap();
        assert_eq!(old.pitch, 24.0);
        assert_eq!(old.lane_width, (3.5, 5.0));
        // R3.20: apartment districts keep their interior whole (no internal
        // roads in the blue estates), like factory/plaza/forest.
        assert!(params_of(BlockType::Apartment).is_none());
        assert!(params_of(BlockType::Factory).is_none());
        assert!(params_of(BlockType::Plaza).is_none());
        assert!(params_of(BlockType::Forest).is_none());
    }

    #[test]
    fn subdivision_is_deterministic() {
        for seed in SEEDS {
            assert_eq!(world(seed), world(seed), "seed {seed}");
        }
    }

    #[test]
    fn only_eligible_types_carry_children() {
        for seed in SEEDS {
            let graph = world(seed);
            for block in &graph.blocks {
                match block.kind {
                    BlockType::Factory
                    | BlockType::Apartment
                    | BlockType::Plaza
                    | BlockType::Forest => {
                        assert!(
                            block.children.is_empty(),
                            "seed {seed}: {:?} block {} was subdivided",
                            block.kind,
                            block.id
                        );
                    }
                    _ => {}
                }
                for child in &block.children {
                    assert_eq!(child.parent, block.id);
                }
            }
        }
    }

    #[test]
    fn village_blocks_are_subdivided_with_the_expected_plot_count() {
        // ADR-0008: a 50 m village cell yields roughly 12-16 plots. R3.20
        // villages are 2-6 cell districts, so the per-BLOCK count scales
        // with the cell count.
        let mut checked = 0;
        let mut total = 0usize;
        let mut cells = 0usize;
        for seed in SEEDS {
            let graph = world(seed);
            for block in &graph.blocks {
                if block.kind != BlockType::UrbanVillage {
                    continue;
                }
                checked += 1;
                total += block.children.len();
                cells += block.cells.len();
                // R3.20b: the OUTER rows of a district stay ONE plot each
                // (ADR-0008 single street frontage), so a narrow 1-wide strip
                // village derives most of its plots from its interior rows
                // only — 14 plots for a 1x3 strip is by design, not a miss.
                // The bound floor therefore relaxes to 4/cell.
                assert!(
                    ((4 * block.cells.len()).max(8)..=30 * block.cells.len())
                        .contains(&block.children.len()),
                    "seed {seed}: village block {} has {} plots for {} cells",
                    block.id,
                    block.children.len(),
                    block.cells.len()
                );
            }
        }
        assert!(checked > 0, "no village blocks in any seed");
        let mean = total as f32 / cells as f32;
        assert!(
            (10.0..=20.0).contains(&mean),
            "mean village plot count per cell {mean} outside the ADR's 12-16 neighbourhood"
        );
    }

    #[test]
    fn plots_never_overlap_and_stay_inside_their_parent() {
        // Flush plots SHARE boundary vertices bit-exactly (the pass snaps
        // them to a 1 mm lattice), so the strict vertex-containment judge
        // of `inset_polygons_never_overlap` false-positives on the shared
        // edge. The SAT penetration depth is the right judge here: edge
        // touching reads 0, genuine interpenetration reads positive — the
        // same epsilon the outer-band wall test uses.
        const OVERLAP_EPSILON: f32 = 0.01;
        // Quantisation snaps vertices to a 1/256 m lattice, so a vertex on a
        // tilted or mitred parent boundary can land up to ~2 mm outside;
        // the pull absorbs that. R3.20 parents are mildly non-convex, and
        // the plot clip-back pass is skipped for them, so allow a slightly
        // larger hair — gross protrusions (the old flipped-normal 5 m kind)
        // are still caught.
        const CONTAIN_EPSILON: f32 = 0.05;
        for seed in SEEDS {
            let graph = world(seed);
            for block in &graph.blocks {
                for (i, a) in block.children.iter().enumerate() {
                    for p in &a.polygon {
                        // Inclusive containment with the lattice slack:
                        // shrink the test point towards the block centre
                        // by the epsilon before the strict test. The judge
                        // is the general simple-polygon test: R3.20 district
                        // polygons can be mildly non-convex (a mid-edge
                        // lattice node dented by its wobble), and the convex
                        // test would reject valid interior points.
                        let center = crate::world::geometry::polygon_centroid(&block.polygon);
                        let to_center = center - *p;
                        let pull = to_center.normalize_or_zero() * CONTAIN_EPSILON;
                        assert!(
                            point_in_polygon(*p + pull, &block.polygon),
                            "seed {seed}: plot vertex {p:?} outside block {}",
                            block.id
                        );
                    }
                    for b in &block.children[i + 1..] {
                        let depth = crate::world::geometry::convex_polygon_overlap_depth(
                            &a.polygon, &b.polygon,
                        );
                        assert!(
                            depth <= OVERLAP_EPSILON,
                            "seed {seed}: plots of block {} interpenetrate by {depth:.4} m",
                            block.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn lane_widths_stay_in_the_type_range_and_under_their_street() {
        for seed in SEEDS {
            let graph = world(seed);
            for edge in &graph.edges {
                if edge.kind != EdgeKind::Lane {
                    continue;
                }
                let block_id = edge.left_block.expect("lane flanks its parent");
                assert_eq!(edge.right_block, Some(block_id));
                let block = &graph.blocks[block_id as usize];
                let params = params_of(block.kind).expect("subdivided type");
                // R3.20: the mouth cap can legitimately pull a lane below its
                // type minimum — a lane ending on a narrow same-kind district
                // seam (down to VILLAGE_STREET_WIDTH_MIN = 2 m) is capped at
                // that seam's width so the mouth never opens wider than the
                // street it ends on.
                let min_width = params
                    .lane_width
                    .0
                    .min(crate::world::VILLAGE_STREET_WIDTH_MIN);
                assert!(
                    edge.width >= min_width - 1e-3 && edge.width <= params.lane_width.1 + 1e-3,
                    "seed {seed}: lane width {} outside {min_width}..{} for {:?}",
                    edge.width,
                    params.lane_width.1,
                    block.kind
                );
                // Mouth cap: never wider than the street it ends on
                // (the arterial counts as a street here, ADR-0009).
                for node in [edge.from, edge.to] {
                    let street_width = graph
                        .edges
                        .iter()
                        .filter(|e| e.kind != EdgeKind::Lane && (e.from == node || e.to == node))
                        .map(|e| e.width)
                        .fold(f32::INFINITY, f32::min);
                    assert!(
                        edge.width <= street_width + 1e-3,
                        "seed {seed}: lane {} wider than its street {street_width}",
                        edge.width
                    );
                }
            }
        }
    }

    #[test]
    fn every_lane_ends_in_two_t_junctions() {
        for seed in SEEDS {
            let graph = world(seed);
            let mut lanes = 0;
            for edge in &graph.edges {
                if edge.kind != EdgeKind::Lane {
                    continue;
                }
                lanes += 1;
                for node in [edge.from, edge.to] {
                    let n = &graph.nodes[node as usize];
                    assert_eq!(n.kind, NodeKind::LaneMouth);
                    // Degree 3: two street halves + this lane = the T.
                    let degree = graph
                        .edges
                        .iter()
                        .filter(|e| e.from == node || e.to == node)
                        .count();
                    assert_eq!(degree, 3, "seed {seed}: mouth {node} degree {degree}");
                    // The mouth sits ON its street's centerline.
                    let on_street = graph.edges.iter().any(|e| {
                        e.kind != EdgeKind::Lane
                            && (e.from == node || e.to == node)
                            && dist_point_to_segment(
                                n.pos,
                                graph.nodes[e.from as usize].pos,
                                graph.nodes[e.to as usize].pos,
                            ) < 1e-2
                    });
                    assert!(on_street, "seed {seed}: mouth {node} off the street line");
                }
            }
            assert!(lanes > 0, "seed {seed}: no lanes at all");
        }
    }

    #[test]
    fn lanes_run_the_full_block_depth() {
        // A lane's centerline must span (nearly) the whole parent polygon
        // along its direction: no broken-up stubs (rejected prototype D).
        for seed in SEEDS {
            let graph = world(seed);
            for edge in &graph.edges {
                if edge.kind != EdgeKind::Lane {
                    continue;
                }
                let block = &graph.blocks[edge.left_block.unwrap() as usize];
                let a = graph.nodes[edge.from as usize].pos;
                let b = graph.nodes[edge.to as usize].pos;
                let dir = (b - a).normalize();
                let (lo, hi) = extent_range(&block.polygon, a, dir);
                let lane_len = a.distance(b);
                // The lane spans the polygon's chord on its own line plus
                // the two half-street setbacks to the centerlines. At a
                // strongly perturbed corner the chord on the lane's line
                // can be several metres shorter than the full extent, and
                // the mouth may slide along the street; two street widths
                // of slack bound that without admitting a stub.
                let slack = 2.0 * block.street_width + 2.0;
                assert!(
                    lane_len >= (hi - lo) - slack,
                    "seed {seed}: lane covers {lane_len:.1} of {:.1} m block depth",
                    hi - lo
                );
                // A lane is never a stub: at least half the block depth.
                assert!(
                    lane_len >= (hi - lo) * 0.5,
                    "seed {seed}: lane {lane_len:.1} m is a stub of {:.1} m",
                    hi - lo
                );
            }
        }
    }

    #[test]
    fn frontages_face_a_lane_at_half_width() {
        for seed in SEEDS {
            let graph = world(seed);
            for block in &graph.blocks {
                for child in &block.children {
                    for f in &child.frontages {
                        let mid = (f.a + f.b) * 0.5;
                        let on_lane = graph.edges.iter().any(|e| {
                            e.kind == EdgeKind::Lane
                                && e.left_block == Some(block.id)
                                && (dist_point_to_segment(
                                    mid,
                                    graph.nodes[e.from as usize].pos,
                                    graph.nodes[e.to as usize].pos,
                                ) - f.lane_half_width)
                                    .abs()
                                    < 1e-2
                        });
                        assert!(on_lane, "seed {seed}: frontage faces no lane");
                    }
                }
            }
        }
    }

    #[test]
    fn walking_scale_junction_spacing_appears() {
        // ADR-0008 verification: after subdivision the graph carries many
        // node-to-node gaps in the walking band (before: zero inside 50 m
        // blocks). Measure street-edge lengths between consecutive nodes.
        for seed in SEEDS {
            let graph = world(seed);
            let mut band = 0;
            for edge in &graph.edges {
                if edge.kind == EdgeKind::Lane {
                    continue;
                }
                let a = graph.nodes[edge.from as usize].pos;
                let b = graph.nodes[edge.to as usize].pos;
                let len = a.distance(b);
                if (8.0..=30.0).contains(&len) {
                    band += 1;
                }
            }
            let mouths = graph
                .nodes
                .iter()
                .filter(|n| n.kind == NodeKind::LaneMouth)
                .count();
            assert!(mouths > 20, "seed {seed}: only {mouths} lane mouths");
            assert!(
                band > 20,
                "seed {seed}: only {band} street gaps in the walking band"
            );
        }
    }

    #[test]
    fn main_axes_are_not_one_directional() {
        // The 30% flip must show up: both lane orientations occur in
        // quantity (regression against the stripe artifact).
        let mut horizontal = 0;
        let mut vertical = 0;
        for seed in SEEDS {
            let graph = world(seed);
            for edge in &graph.edges {
                if edge.kind != EdgeKind::Lane {
                    continue;
                }
                let a = graph.nodes[edge.from as usize].pos;
                let b = graph.nodes[edge.to as usize].pos;
                let d = (b - a).abs();
                if d.x > d.y {
                    horizontal += 1;
                } else {
                    vertical += 1;
                }
            }
        }
        let total = (horizontal + vertical) as f32;
        let share = horizontal as f32 / total;
        assert!(
            (0.2..=0.8).contains(&share),
            "lane axes lopsided: {horizontal} h / {vertical} v"
        );
    }

    #[test]
    fn tier0_streets_keep_their_lines_and_widths() {
        // Every street edge (split halves included) must connect valid
        // node kinds, and block street widths stay in their type range:
        // the pass adds T junctions, never rewrites tier-0 metadata.
        for seed in SEEDS {
            let graph = world(seed);
            for edge in &graph.edges {
                if edge.kind == EdgeKind::Lane {
                    continue;
                }
                let a = graph.nodes[edge.from as usize].pos;
                let b = graph.nodes[edge.to as usize].pos;
                assert!(a.distance(b) > 0.5, "seed {seed}: degenerate street half");
                for node in [edge.from, edge.to] {
                    match graph.nodes[node as usize].kind {
                        // R3.20: pass anchors join collinear unit edges.
                        NodeKind::Junction
                        | NodeKind::Corner
                        | NodeKind::Pass
                        | NodeKind::LaneMouth => {}
                        other => panic!("street endpoint kind {other:?}"),
                    }
                }
            }
            for block in &graph.blocks {
                let (min, max) = match block.kind {
                    BlockType::UrbanVillage => (
                        crate::world::VILLAGE_STREET_WIDTH_MIN,
                        crate::world::VILLAGE_STREET_WIDTH_MAX,
                    ),
                    _ => (
                        crate::world::STREET_WIDTH_MIN,
                        crate::world::STREET_WIDTH_MAX,
                    ),
                };
                assert!((min..=max).contains(&block.street_width));
            }
        }
    }
}
