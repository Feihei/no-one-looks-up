//! Geometry helpers for block polygons (ADR-0004).
//!
//! Block polygons come from the grid as axis-aligned convex rectangles
//! (blocks are carved as rectangles; see `path_network`). The inset uses a
//! self-written half-plane clip (`inset_polygon_clip`), which for convex
//! polygons mathematically guarantees the inset stays inside the original
//! polygon. Because adjacent blocks share the street centerline and each
//! insets by `width / 2` on its own territory, neighboring inset polygons
//! never overlap — asserted by the OVERLAPS=0 tests here and in
//! `path_network`.

use bevy::math::Vec2;

/// Minimum polygon area (m²) for an inset result to be considered valid.
const MIN_AREA: f32 = 1.0;

/// Maximum miter stretch factor (`1/cos(θ/2)`) at a polygon vertex. Sharp
/// perturbed corners would otherwise push the inset vertex far past the
/// street centerline and into the neighbour.
const MITER_LIMIT: f32 = 1.5;

/// Clips `polygon` (convex, CCW winding) against the half-plane
/// `normal · p >= d`, keeping the inside (Sutherland–Hodgman step).
pub(crate) fn clip_half_plane(polygon: &[Vec2], normal: Vec2, d: f32) -> Vec<Vec2> {
    let mut out = Vec::with_capacity(polygon.len() + 1);
    let n = polygon.len();
    for i in 0..n {
        let cur = polygon[i];
        let next = polygon[(i + 1) % n];
        let cur_side = normal.dot(cur) - d;
        let next_side = normal.dot(next) - d;
        if cur_side >= 0.0 {
            out.push(cur);
        }
        // Edge crosses the plane: emit the intersection point.
        if (cur_side > 0.0 && next_side < 0.0) || (cur_side < 0.0 && next_side > 0.0) {
            let t = cur_side / (cur_side - next_side);
            out.push(cur.lerp(next, t));
        }
    }
    out
}

/// Signed area of a polygon (positive for CCW winding in standard math axes).
pub fn polygon_area(polygon: &[Vec2]) -> f32 {
    let n = polygon.len();
    let mut sum = 0.0;
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        sum += a.x * b.y - b.x * a.y;
    }
    sum * 0.5
}

/// Inward unit normal per edge of a simple polygon, derived from its winding
/// (positive signed area = interior on the left of each edge).
///
/// This replaces a centroid-direction heuristic: for a tall narrow polygon
/// (a 6 m x 250 m village plot strip) the centroid sits almost due north of
/// a west-face edge midpoint, and the nearly-horizontal normal's dot with
/// that offset takes its sign from the tiny vertical component — the normal
/// flipped, faces inset OUTWARD and the collision hull poked ~5 m into the
/// street. The winding is exact for any simple polygon.
pub fn inward_normals(polygon: &[Vec2]) -> Vec<Vec2> {
    let ccw = polygon_area(polygon) > 0.0;
    let n = polygon.len();
    let mut normals = Vec::with_capacity(n);
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let edge = b - a;
        if edge.length_squared() < 1e-12 {
            normals.push(Vec2::ZERO);
            continue;
        }
        let normal = if ccw {
            Vec2::new(-edge.y, edge.x)
        } else {
            Vec2::new(edge.y, -edge.x)
        };
        normals.push(normal.normalize());
    }
    normals
}

/// Whether every interior angle of the polygon turns the same way (weakly
/// convex: collinear vertices allowed). R3.20 district polygons are mildly
/// non-convex — a mid-edge lattice node dents by its ±4 m wobble.
pub fn polygon_is_convex(polygon: &[Vec2]) -> bool {
    let n = polygon.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0.0_f32;
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let c = polygon[(i + 2) % n];
        let cross = (b - a).perp_dot(c - b);
        if cross.abs() < 1e-9 {
            continue;
        }
        let s = cross.signum();
        if sign == 0.0 {
            sign = s;
        } else if s != sign {
            return false;
        }
    }
    true
}

/// Splits a simple polygon into convex pieces (ear-clipping triangulation).
///
/// Collision prisms assume convexity (`push_out` slides along one supporting
/// edge), and R3.20's district/plot outlines are only weakly non-convex —
/// a mid-edge lattice node dents by its ±4 m wobble. The old fix (clip the
/// convex hull by the polygon's own edge half-planes) fails both ways on a
/// strongly non-convex outline: a 6 m x 250 m village plot strip's kernel
/// excludes its own centroid, so either the prism lost the plot's solid
/// centre or the unclipped hull bridged a face dip ~5 m into the street.
/// Triangles are always convex and always inside the polygon, so both hold.
///
/// Convex input passes through as one piece; a degenerate clip (collinear
/// vertices on quantised outlines) returns the input whole as a fallback.
///
/// Input hygiene (R3.20): block and plot outlines are stored CLOSED
/// (`last == first`), and a duplicated vertex turns the simple-polygon
/// invariant of ear clipping into a lie — measured, a 6-vertex closed
/// pentagon produced a "final" triangle spanning its concave notch (the
/// triangle's west face ran into the street and pinned the walker) plus a
/// zero-area `[A, D, A]` shard. So the vertex ring is deduplicated first
/// (closing vertex and consecutive near-duplicates), and degenerate
/// triangles are dropped from the output.
pub fn convex_decompose(polygon: &[Vec2]) -> Vec<Vec<Vec2>> {
    if polygon.len() < 3 {
        return Vec::new();
    }
    let mut ring: Vec<Vec2> = Vec::with_capacity(polygon.len());
    for &v in polygon {
        if ring
            .last()
            .is_none_or(|last| (*last - v).length_squared() > 1e-10)
        {
            ring.push(v);
        }
    }
    // Drop the duplicated closing vertex of a stored-closed outline.
    while ring.len() > 1 {
        let first = ring[0];
        let last = ring[ring.len() - 1];
        if (first - last).length_squared() > 1e-10 {
            break;
        }
        ring.pop();
    }
    if ring.len() < 3 {
        return Vec::new();
    }
    let polygon = ring.as_slice();
    if polygon_is_convex(polygon) {
        return vec![polygon.to_vec()];
    }
    // Ear clipping wants CCW winding.
    let mut indices: Vec<usize> = (0..polygon.len()).collect();
    if polygon_area(polygon) < 0.0 {
        indices.reverse();
    }
    let mut triangles: Vec<Vec<Vec2>> = Vec::new();
    let mut guard = 0_usize;
    while indices.len() > 3 && guard < 4 * polygon.len() {
        guard += 1;
        let mut clipped = false;
        for k in 0..indices.len() {
            let i0 = indices[(k + indices.len() - 1) % indices.len()];
            let i1 = indices[k];
            let i2 = indices[(k + 1) % indices.len()];
            let (a, b, c) = (polygon[i0], polygon[i1], polygon[i2]);
            // The ear tip must be convex (CCW turn).
            if (b - a).perp_dot(c - b) <= 1e-9 {
                continue;
            }
            // And no other vertex may fall inside the ear.
            let mut occupied = false;
            for &j in &indices {
                if j == i0 || j == i1 || j == i2 {
                    continue;
                }
                let p = polygon[j];
                if on_segment(a, b, p) || on_segment(b, c, p) || on_segment(c, a, p) {
                    continue; // riding the ear's edge: not blocking
                }
                if point_in_polygon(p, &[a, b, c]) {
                    occupied = true;
                    break;
                }
            }
            if occupied {
                continue;
            }
            triangles.push(vec![a, b, c]);
            indices.remove(k);
            clipped = true;
            break;
        }
        if !clipped {
            break;
        }
    }
    if indices.len() == 3 {
        let tri: Vec<Vec2> = indices.iter().map(|&i| polygon[i]).collect();
        // A collinear leftover ("triangle") has no interior to protect.
        if (tri[1] - tri[0]).perp_dot(tri[2] - tri[1]) > 1e-9 {
            triangles.push(tri);
        }
    } else if triangles.is_empty() {
        // Degenerate outline: hand the input through untouched rather than
        // dropping the collider entirely.
        return vec![polygon.to_vec()];
    }
    triangles
}

/// Whether `p` sits on segment `ab` (within a small epsilon).
fn on_segment(a: Vec2, b: Vec2, p: Vec2) -> bool {
    let ab = b - a;
    if ab.length_squared() < 1e-12 {
        return (p - a).length_squared() < 1e-8;
    }
    let t = (p - a).dot(ab) / ab.length_squared();
    (0.0..=1.0).contains(&t) && (p - a.lerp(b, t)).length_squared() < 1e-6
}

/// Insets a convex polygon by a per-edge amount (ADR-0004).
///
/// Each vertex is moved along the **angle bisector of its two incident
/// edges**, inward, scaled so both edges end up exactly `inset` away from
/// where they were. Working per-vertex (not per-line-intersection) keeps the
/// result strictly inside the original polygon even when perturbation makes
/// consecutive edges non-orthogonal — the failure mode that produced up to
/// 20 m of overlap when the offset lines were intersected independently.
///
/// `insets[i]` is the inset for edge `i` (`polygon[i] -> polygon[i + 1]`); a
/// vertex between two edges uses the average of the two, which is exact for
/// equal insets (the normal case) and conservative otherwise.
///
/// Returns `None` if the polygon degenerates.
pub fn inset_polygon_clip_with_insets(polygon: &[Vec2], insets: &[f32]) -> Option<Vec<Vec2>> {
    debug_assert_eq!(polygon.len(), insets.len(), "one inset per edge");
    let n = polygon.len();
    if n < 3 {
        return None;
    }

    // Inward unit normal per edge, from the winding (see [`inward_normals`]).
    let normals = inward_normals(polygon);

    let mut out = Vec::with_capacity(n);
    for (i, vertex) in polygon.iter().enumerate() {
        let prev_edge = (i + n - 1) % n;
        let cur_edge = i;
        let inset = (insets[prev_edge] + insets[cur_edge]) * 0.5;
        // Bisector of the two inward normals, normalized; its length factor
        // 1/cos(θ/2) keeps both edges at the requested distance.
        let mut dir = normals[prev_edge] + normals[cur_edge];
        if dir.length_squared() < 1e-9 {
            // Antiparallel normals (180° spike): fall back to one normal.
            dir = normals[cur_edge];
        }
        let dir = dir.normalize();
        // Miter length is 1/cos(θ/2); capping the stretch keeps a sharp
        // perturbed corner from shooting a vertex far past the street.
        // A cap of 1.5 costs a slightly short edge at that vertex (the street
        // there is a bit wider) instead of cutting into the neighbour.
        let cos_half = dir.dot(normals[cur_edge]).clamp(1.0 / MITER_LIMIT, 1.0);
        out.push(*vertex + dir * (inset / cos_half));
    }

    if polygon_area(&out).abs() < MIN_AREA {
        return None;
    }
    Some(out)
}

/// Convex hull of a point set (monotone chain), CCW, collinear points
/// removed. Used to keep block polygons convex: an L-shaped block's perimeter
/// walk contains a reflex corner that would turn inside-out once inset.
pub fn convex_hull(points: &[Vec2]) -> Vec<Vec2> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal))
    });
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    fn cross(o: Vec2, a: Vec2, b: Vec2) -> f32 {
        (a - o).perp_dot(b - o)
    }
    let mut lower: Vec<Vec2> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<Vec2> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Arithmetic mean of the polygon's vertices (the original's
/// `_polygon_center`; sufficient as an interior reference point for
/// determining inward normals of a convex polygon).
pub fn polygon_centroid(polygon: &[Vec2]) -> Vec2 {
    let n = polygon.len().max(1) as f32;
    polygon.iter().copied().sum::<Vec2>() / n
}

/// Insets a convex polygon by `inset` meters via half-plane clipping.
///
/// `polygon` must be convex and wound CCW. Returns `None` if the polygon
/// collapses below [`MIN_AREA`] (inset larger than the polygon supports).
pub fn inset_polygon_clip(polygon: &[Vec2], inset: f32) -> Option<Vec<Vec2>> {
    debug_assert!(inset >= 0.0, "inset must be non-negative");
    debug_assert!(polygon.len() >= 3, "polygon needs at least 3 vertices");
    let mut current: Vec<Vec2> = polygon.to_vec();
    let n = polygon.len();
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let edge = b - a;
        let len = edge.length();
        if len < f32::EPSILON {
            continue;
        }
        // CCW winding: the inside of the polygon is to the left of each edge,
        // i.e. along the edge's left normal.
        let normal = Vec2::new(-edge.y, edge.x) / len;
        let d = normal.dot(a + normal * inset);
        current = clip_half_plane(&current, normal, d);
        if current.len() < 3 {
            return None;
        }
    }
    if polygon_area(&current) < MIN_AREA {
        return None;
    }
    Some(current)
}

/// Shortest distance from point `p` to the segment `a..b` (m).
pub fn dist_point_to_segment(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    if ab.length_squared() < 1e-12 {
        return (p - a).length();
    }
    let t = ((p - a).dot(ab) / ab.dot(ab)).clamp(0.0, 1.0);
    (p - (a + ab * t)).length()
}

/// Shortest distance between two segments `p1..p2` and `q1..q2` (m).
///
/// `0` when they cross. Used by placement to reject a footprint that grazes a
/// street centerline without any corner landing in it.
pub fn dist_segment_to_segment(p1: Vec2, p2: Vec2, q1: Vec2, q2: Vec2) -> f32 {
    if segments_intersect(p1, p2, q1, q2) {
        return 0.0;
    }
    dist_point_to_segment(p1, q1, q2)
        .min(dist_point_to_segment(p2, q1, q2))
        .min(dist_point_to_segment(q1, p1, p2))
        .min(dist_point_to_segment(q2, p1, p2))
}

/// Crossing-number test in world coordinates: `true` for a point inside a
/// simple polygon of any winding (the original `_is_point_in_polygon`).
pub fn point_in_polygon(p: Vec2, polygon: &[Vec2]) -> bool {
    let n = polygon.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (polygon[i], polygon[j]);
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Distance floor (m) below which the footprint is rejected outright, so
/// float noise on a shared edge cannot flip a decision.
const ROUNDING_FLOOR: f32 = 1e-4;

/// Proper or touching intersection of two segments (endpoints count).
pub fn segments_intersect(p1: Vec2, p2: Vec2, q1: Vec2, q2: Vec2) -> bool {
    let cross = |a: Vec2, b: Vec2, c: Vec2| (b - a).perp_dot(c - a);
    let d1 = cross(q1, q2, p1);
    let d2 = cross(q1, q2, p2);
    let d3 = cross(p1, p2, q1);
    let d4 = cross(p1, p2, q2);
    let straddle = |a: f32, b: f32| {
        (a > ROUNDING_FLOOR && b < -ROUNDING_FLOOR) || (a < -ROUNDING_FLOOR && b > ROUNDING_FLOOR)
    };
    if straddle(d1, d2) && straddle(d3, d4) {
        return true;
    }
    let on_segment = |a: Vec2, b: Vec2, c: Vec2| {
        c.x >= a.x.min(b.x) - ROUNDING_FLOOR
            && c.x <= a.x.max(b.x) + ROUNDING_FLOOR
            && c.y >= a.y.min(b.y) - ROUNDING_FLOOR
            && c.y <= a.y.max(b.y) + ROUNDING_FLOOR
    };
    (d1.abs() <= ROUNDING_FLOOR && on_segment(q1, q2, p1))
        || (d2.abs() <= ROUNDING_FLOOR && on_segment(q1, q2, p2))
        || (d3.abs() <= ROUNDING_FLOOR && on_segment(p1, p2, q1))
        || (d4.abs() <= ROUNDING_FLOOR && on_segment(p1, p2, q2))
}

/// True if an axis-aligned footprint (center + half extents) lies entirely
/// inside a simple polygon and never crosses its boundary.
///
/// This is the convex-hull companion to [`point_in_convex_polygon`]: the
/// visible block outline can be slightly concave (R3.4), while collision uses
/// the hull, so a footprint must stay inside *both*.
pub fn rect_inside_polygon(center: Vec2, half: Vec2, polygon: &[Vec2]) -> bool {
    let corners = [
        Vec2::new(center.x - half.x, center.y - half.y),
        Vec2::new(center.x + half.x, center.y - half.y),
        Vec2::new(center.x + half.x, center.y + half.y),
        Vec2::new(center.x - half.x, center.y + half.y),
    ];
    if !corners
        .iter()
        .all(|c| point_in_polygon(*c, polygon) || point_in_convex_inclusive(*c, polygon))
    {
        return false;
    }
    let n = polygon.len();
    (0..4).all(|i| {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        (0..n).all(|j| !segments_intersect(a, b, polygon[j], polygon[(j + 1) % n]))
    })
}

/// Footprint corners of a rectangle rotated by `yaw` about `center`.
///
/// The winding matches [`crate::world::placement::rotated_corners`] and
/// therefore Bevy's `Quat::from_rotation_y` -- a placement test that disagrees
/// with the drawn transform is how a whole block type silently disappears.
pub fn oriented_rect_corners(center: Vec2, half: Vec2, yaw: f32) -> [Vec2; 4] {
    let (s, c) = yaw.sin_cos();
    let offset =
        |lx: f32, lz: f32| Vec2::new(center.x + lx * c + lz * s, center.y - lx * s + lz * c);
    [
        offset(-half.x, -half.y),
        offset(half.x, -half.y),
        offset(half.x, half.y),
        offset(-half.x, half.y),
    ]
}

/// True if a footprint rotated by `yaw` lies entirely inside a simple polygon
/// and never crosses its boundary.
///
/// This exists because the axis-aligned version was wrong the moment buildings
/// started facing the street: `placement` draws every building at a yaw that
/// points its length along the street, so a 25 m apartment is a 25 x 12 m
/// rectangle turned 90 degrees from the axis-aligned box the old test measured.
/// The test has to use the same corners the renderer does.
pub fn oriented_rect_inside_polygon(center: Vec2, half: Vec2, yaw: f32, polygon: &[Vec2]) -> bool {
    let corners = oriented_rect_corners(center, half, yaw);
    if !corners
        .iter()
        .all(|c| point_in_polygon(*c, polygon) || point_in_convex_inclusive(*c, polygon))
    {
        return false;
    }
    let n = polygon.len();
    (0..4).all(|i| {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        (0..n).all(|j| !segments_intersect(a, b, polygon[j], polygon[(j + 1) % n]))
    })
}

/// True if a point lies inside or on the boundary of a convex polygon.
///
/// Placement rejects a footprint whose corner sits exactly on a curb, so the
/// boundary must count as inside. The half-plane test uses a signed margin:
/// with CCW winding every edge must satisfy `cross(edge, p) >= -eps`.
pub fn point_in_convex_inclusive(p: Vec2, polygon: &[Vec2]) -> bool {
    let n = polygon.len();
    if n < 3 {
        return false;
    }
    // Resolve winding from the signed area so CW input works too.
    let area: f32 = (0..n)
        .map(|i| {
            let a = polygon[i];
            let b = polygon[(i + 1) % n];
            a.x * b.y - b.x * a.y
        })
        .sum();
    let sign = if area >= 0.0 { 1.0 } else { -1.0 };
    (0..n).all(|i| {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        let edge = b - a;
        edge.perp_dot(p - a) * sign >= -ON_EDGE_EPSILON * edge.length()
    })
}

/// Distance from the boundary that still counts as "inside" (m).
const ON_EDGE_EPSILON: f32 = 1e-4;

/// True if a point lies strictly inside a convex polygon (ray casting).
pub fn point_in_convex_polygon(p: Vec2, polygon: &[Vec2]) -> bool {
    let n = polygon.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (a, b) = (polygon[i], polygon[j]);
        if (b.y > p.y) != (a.y > p.y) && p.x < (a.x - b.x) * (p.y - b.y) / (a.y - b.y) + b.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// True if two convex polygons genuinely interpenetrate.
///
/// This is the OVERLAPS=0 judge for block polygons. It checks a vertex of one
/// inside the other; two convex polygons that share no vertex yet overlap must
/// cross edges, which for our tiling cannot happen, so this is exact for the
/// case that matters and immune to the false positives SAT produces when
/// two perturbed edges run within millimeters of each other while staying on
/// their own side of the street.
pub fn convex_polygons_interpenetrate(a: &[Vec2], b: &[Vec2]) -> bool {
    a.iter().any(|p| point_in_convex_polygon(*p, b))
        || b.iter().any(|p| point_in_convex_polygon(*p, a))
}

/// Penetration depth between two convex polygons, in meters (SAT).
///
/// `0` means the polygons are disjoint **or** merely touch along an edge (the
/// enclosure band's flush, zero-gap joint). A positive value is how deep the
/// two shapes interpenetrate — the real overlap the OVERLAPS=0 guarantee
/// forbids.
pub fn convex_polygon_overlap_depth(a: &[Vec2], b: &[Vec2]) -> f32 {
    fn axis_depth(a: &[Vec2], b: &[Vec2], axis: Vec2) -> f32 {
        let (a_min, a_max) = project_polygon(a, axis);
        let (b_min, b_max) = project_polygon(b, axis);
        a_max.min(b_max) - a_min.max(b_min)
    }
    fn project_polygon(polygon: &[Vec2], axis: Vec2) -> (f32, f32) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for p in polygon {
            let d = axis.dot(*p);
            min = min.min(d);
            max = max.max(d);
        }
        (min, max)
    }
    let mut depth = f32::INFINITY;
    for poly in [a, b] {
        let n = poly.len();
        for i in 0..n {
            let edge = poly[(i + 1) % n] - poly[i];
            if edge.length_squared() < 1e-12 {
                continue;
            }
            let axis = Vec2::new(-edge.y, edge.x).normalize();
            let d = axis_depth(a, b, axis);
            if d <= 0.0 {
                return 0.0; // Separating axis: disjoint or edge-touching.
            }
            depth = depth.min(d);
        }
    }
    if depth.is_finite() { depth } else { 0.0 }
}

/// Convex polygons overlap by more than `epsilon` meters.
pub fn convex_polygons_overlap_by(a: &[Vec2], b: &[Vec2], epsilon: f32) -> bool {
    convex_polygon_overlap_depth(a, b) > epsilon
}

/// True if two convex polygons overlap (separating axis test on edge normals).
pub fn convex_polygons_overlap(a: &[Vec2], b: &[Vec2]) -> bool {
    fn sat_overlap(a: &[Vec2], b: &[Vec2]) -> bool {
        let n = a.len();
        for i in 0..n {
            let edge = a[(i + 1) % n] - a[i];
            if edge.length_squared() < f32::EPSILON * f32::EPSILON {
                continue;
            }
            // Axis perpendicular to the edge.
            let axis = Vec2::new(-edge.y, edge.x);
            let (a_min, a_max) = project(a, axis);
            let (b_min, b_max) = project(b, axis);
            if a_max < b_min || b_max < a_min {
                return false; // Separating axis found.
            }
        }
        true
    }
    fn project(polygon: &[Vec2], axis: Vec2) -> (f32, f32) {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for p in polygon {
            let d = axis.dot(*p);
            min = min.min(d);
            max = max.max(d);
        }
        (min, max)
    }
    sat_overlap(a, b) && sat_overlap(b, a)
}

/// Corner points (world coordinates, CCW) of the rectangle covering the cell
/// range `[row_start, row_end) x [col_start, col_end)`.
pub fn cell_rect_polygon(rows: std::ops::Range<u32>, cols: std::ops::Range<u32>) -> Vec<Vec2> {
    let min_x = cols.start as f32 * crate::world::CELL_SIZE;
    let max_x = cols.end as f32 * crate::world::CELL_SIZE;
    // Row 0 is the northernmost band (min Z in world space).
    let min_z = rows.start as f32 * crate::world::CELL_SIZE;
    let max_z = rows.end as f32 * crate::world::CELL_SIZE;
    vec![
        Vec2::new(min_x, min_z),
        Vec2::new(max_x, min_z),
        Vec2::new(max_x, max_z),
        Vec2::new(min_x, max_z),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(half: f32) -> Vec<Vec2> {
        vec![
            Vec2::new(-half, -half),
            Vec2::new(half, -half),
            Vec2::new(half, half),
            Vec2::new(-half, half),
        ]
    }

    #[test]
    fn inset_square_shrinks_uniformly() {
        let inset = inset_polygon_clip(&square(25.0), 3.0).expect("inset should not collapse");
        assert!((polygon_area(&inset) - 44.0 * 44.0).abs() < 1e-3);
        for p in &inset {
            assert!(p.x.abs() <= 22.0 + 1e-3 && p.y.abs() <= 22.0 + 1e-3);
        }
    }

    #[test]
    fn inset_too_large_collapses_to_none() {
        assert!(inset_polygon_clip(&square(25.0), 25.0).is_none());
        assert!(inset_polygon_clip(&square(25.0), 100.0).is_none());
    }

    #[test]
    fn l_shaped_block_rectangle_decomposition_insets_without_overlap() {
        // Two adjacent rectangles belonging to the same block inset
        // independently; the pair of parts stays inside the original union.
        let a = cell_rect_polygon(0..1, 0..2);
        let b = cell_rect_polygon(1..2, 0..1);
        let ia = inset_polygon_clip(&a, 3.0).expect("part a inset");
        let ib = inset_polygon_clip(&b, 3.0).expect("part b inset");
        for p in &ia {
            assert!(
                p.x >= 3.0 - 1e-3 && p.x <= 97.0 + 1e-3 && p.y >= 3.0 - 1e-3 && p.y <= 47.0 + 1e-3
            );
        }
        for p in &ib {
            assert!(
                p.x >= 3.0 - 1e-3 && p.x <= 47.0 + 1e-3 && p.y >= 53.0 - 1e-3 && p.y <= 97.0 + 1e-3
            );
        }
    }

    #[test]
    fn overlapping_squares_detected() {
        assert!(convex_polygons_overlap(&square(10.0), &square(10.0)));
        let shifted: Vec<Vec2> = square(10.0)
            .iter()
            .map(|p| Vec2::new(p.x + 15.0, p.y))
            .collect();
        assert!(convex_polygons_overlap(&square(10.0), &shifted));
        let far: Vec<Vec2> = square(10.0)
            .iter()
            .map(|p| Vec2::new(p.x + 100.0, p.y))
            .collect();
        assert!(!convex_polygons_overlap(&square(10.0), &far));
    }

    #[test]
    fn touching_edges_do_not_count_as_overlap() {
        // Shared street centerline: two blocks inset from a common edge do
        // not overlap (the OVERLAPS=0 guarantee).
        let left = cell_rect_polygon(0..1, 0..1);
        let right = cell_rect_polygon(0..1, 1..2);
        let il = inset_polygon_clip(&left, 3.0).expect("left inset");
        let ir = inset_polygon_clip(&right, 3.0).expect("right inset");
        assert!(!convex_polygons_overlap(&il, &ir));
    }
}
