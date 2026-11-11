//! Roof silhouettes (R3.11 step 11.3).
//!
//! ADR-0001: the skyline is made of trees and building tops, watched from the
//! shoulders up. The roofline is therefore the single highest-value strip of
//! geometry in the city, and it is also the strip the facade texture cannot
//! provide -- a wall texture stops at the parapet.
//!
//! # Three roofs, one per style
//!
//! | Style | Roof | Why |
//! |---|---|---|
//! | 老城 | 双坡 gable, eave overhang baked | ADR-0001: "低矮坡屋顶"; a ridge along the street keeps the skyline a clean saw-free band and the gable ends face the side streets |
//! | 旧公寓 | parapet ring (女儿墙) | the 1980s slab's flat top with a 0.9 m wall is its whole identity from below |
//! | 旧厂房 | sawtooth (锯齿) | ADR-0001: "高耸体量"; teeth repeating along the FRONTAGE cut the silhouette into the zigzag that reads as a factory |
//!
//! # Roofs are dark, on purpose
//!
//! Every tint here is darker than its style's wall tint. The read at dusk is a
//! dark mass against a bright sky; a roof as bright as its wall would merge the
//! two and the building would lose its top. `tint_is_darker_than_its_wall` pins
//! that, and `silhouette_budget` pins the absolute value.
//!
//! # Meshes are unit-scaled, not built per building
//!
//! Like the shared cube: one gable mesh and one sawtooth tooth for the whole
//! city, scaled per instance. The one place this bites is the parapet, whose
//! wall thickness must stay ~0.25 m absolute -- a baked thickness would scale
//! with the building and end up metres thick -- so the parapet is four instances
//! of the shared unit cube instead of a baked ring.
//!
//! # Winding is asserted, not assumed
//!
//! wgpu's default (and Bevy's, which does not override it) is
//! `FrontFace::Ccw`: a triangle is visible when its vertices run
//! counter-clockwise seen from outside, which is the same order the right-hand
//! rule gives. `every_triangle_faces_outward` recomputes each triangle's normal
//! from its own vertices and requires it to point away from the mesh -- a
//! single inverted face would otherwise show as a hole in the skyline that no
//! unit test but this one can see.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, Mesh, PrimitiveTopology};
use bevy::prelude::*;

use crate::world::facade::FacadeStyle;

/// Sawtooth pitch along the frontage (m). A 42-48 m factory gets 7-8 teeth.
pub const SAWTOOTH_PITCH: f32 = 6.0;

/// How a style's roof is built.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoofProfile {
    pub kind: RoofKind,
    /// Rise above the wall top (m), before the per-variant jitter.
    pub rise: f32,
    /// Sawtooth pitch along the frontage (m); unused by the other kinds.
    pub pitch: f32,
    /// Roof tint, sRGB. Darker than the style's wall tint by design.
    pub tint: [f32; 3],
}

/// Roof geometry kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoofKind {
    /// 双坡 gable: ridge along the frontage, eaves overhanging the depth.
    Gable,
    /// Parapet ring: four walls on a flat deck.
    Parapet,
    /// Sawtooth: wedge teeth repeating along the frontage.
    Sawtooth,
}

impl RoofProfile {
    pub fn of(style: FacadeStyle) -> Self {
        match style {
            // A 1.1 m rise on a 4-6 m shophouse is the classic 双坡 proportion.
            FacadeStyle::OldCity => Self {
                kind: RoofKind::Gable,
                rise: 1.1,
                pitch: 0.0,
                tint: [0.30, 0.26, 0.22],
            },
            FacadeStyle::Apartment => Self {
                kind: RoofKind::Parapet,
                rise: 0.9,
                pitch: 0.0,
                tint: [0.42, 0.42, 0.44],
            },
            // 2.2 m teeth over a 24 m shed: tall enough to cut the skyline from
            // across a 10 m street at the 22.55 degree minimum pitch.
            FacadeStyle::Factory => Self {
                kind: RoofKind::Sawtooth,
                rise: 2.2,
                pitch: SAWTOOTH_PITCH,
                tint: [0.22, 0.22, 0.24],
            },
        }
    }

    /// Rise for one building, jittered by its facade variant so a street of
    /// identical shophouses does not end on one shared ridge height.
    pub fn rise_for(self, variant: u8) -> f32 {
        match variant {
            0 => self.rise * 0.9,
            _ => self.rise * 1.15,
        }
    }

    /// Sawtooth teeth along a frontage, tiling it exactly.
    ///
    /// Returns `(count, tooth_width)`. The count is the nearest whole number of
    /// pitches and the width absorbs the remainder, so the teeth always cover
    /// the frontage edge to edge -- a fixed pitch would leave a gap at one end
    /// that reads as a missing tooth from the street.
    pub fn teeth(self, frontage: f32) -> (u32, f32) {
        let count = ((frontage / self.pitch).round() as u32).max(1);
        (count, frontage / count as f32)
    }
}

// --- mesh baking -----------------------------------------------------------

/// Accumulates triangles with flat per-face normals and simple UVs.
///
/// Shared with `world::props`, which bakes its awning/balcony/lamp/wire
/// meshes with the same recipe (flat normals, dummy UVs, CCW winding).
#[derive(Default)]
pub(crate) struct MeshBuf {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    indices: Vec<u32>,
}

impl MeshBuf {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// One triangle, corners ordered CCW seen from outside.
    pub(crate) fn tri(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3]) {
        let n = face_normal(a, b, c);
        let base = self.positions.len() as u32;
        self.positions.extend_from_slice(&[a, b, c]);
        self.normals.extend_from_slice(&[n; 3]);
        // Plain materials sample nothing here, but StandardMaterial requires
        // UV_0 to exist for its flags; a per-face 0..1 ramp is enough.
        self.uvs
            .extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]);
        self.indices.extend_from_slice(&[base, base + 1, base + 2]);
    }

    /// One quad, corners ordered CCW seen from outside.
    pub(crate) fn quad(&mut self, a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3]) {
        self.tri(a, b, c);
        self.tri(a, c, d);
    }

    pub(crate) fn build(self) -> Mesh {
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, self.positions);
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, self.uvs);
        mesh.insert_indices(Indices::U32(self.indices));
        mesh
    }
}

/// Right-hand-rule normal of a CCW triangle: the direction wgpu's
/// `FrontFace::Ccw` treats as the front.
fn face_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        ab[1] * ac[2] - ab[2] * ac[1],
        ab[2] * ac[0] - ab[0] * ac[2],
        ab[0] * ac[1] - ab[1] * ac[0],
    ]
}

/// The 双坡 gable, normalised: base on y = 0, ridge on y = 1 running along x,
/// eaves overhanging z by 6% of the depth each side.
///
/// Baked once for the whole city and scaled by `(frontage, rise, depth)` per
/// building, so the overhang is proportional to the depth -- 0.24-0.36 m on a
/// 4-6 m shophouse, which is the eave the style calls for.
///
/// The shell is **closed**, including a soffit at y = 0. The eave hangs past
/// the wall, and a walker standing under it looks straight up into the slope's
/// back face: with back-face culling that is a hole in the roof, and through
/// the hole, sky. The soffit is the underside the overhang was always missing
/// (`gable_is_a_closed_shell` pins it).
pub fn bake_gable() -> Mesh {
    let (x0, x1) = (-0.5, 0.5);
    let z_eave = 0.56;
    let mut buf = MeshBuf::new();
    // Slopes. The ridge runs along x at z = 0; each slope drains to one eave.
    buf.quad(
        [x0, 0.0, z_eave],
        [x1, 0.0, z_eave],
        [x1, 1.0, 0.0],
        [x0, 1.0, 0.0],
    );
    buf.quad(
        [x1, 0.0, -z_eave],
        [x0, 0.0, -z_eave],
        [x0, 1.0, 0.0],
        [x1, 1.0, 0.0],
    );
    // 山墙: the gable-end walls. They run all the way to the EAVE, not just to
    // the wall line: the slope's edge ends at z = ±z_eave, so capping at
    // z_wall left a thin triangular slit of sky between the roof plane and the
    // gable wall at each end (visible as a bright wedge from the side street).
    // Reaching the eave closes it and reads as a rake board.
    buf.tri([x1, 0.0, z_eave], [x1, 0.0, -z_eave], [x1, 1.0, 0.0]);
    buf.tri([x0, 0.0, -z_eave], [x0, 0.0, z_eave], [x0, 1.0, 0.0]);
    // 檐底 soffit: shuts the triangular prism from below. Coplanar with the
    // wall top, which costs nothing: seen from below the wall's top face is
    // culled and seen from above this one is, so the two never both rasterise.
    buf.quad(
        [x0, 0.0, -z_eave],
        [x1, 0.0, -z_eave],
        [x1, 0.0, z_eave],
        [x0, 0.0, z_eave],
    );

    buf.build()
}

/// One sawtooth tooth, normalised: wedge cross-section in the x-y plane,
/// extruded along the depth.
///
/// The glazed vertical face sits at the tooth's +x boundary and the slope
/// rises from -x toward it, so a street of teeth reads /|/|/| from the side --
/// the silhouette ADR-0001 calls "高耸体量". Teeth repeat along the FRONTAGE
/// (local x), which is why the roofline cuts into the sky from the street.
pub fn bake_tooth() -> Mesh {
    let (x0, x1) = (-0.5, 0.5);
    let (z0, z1) = (-0.5, 0.5);
    let mut buf = MeshBuf::new();

    // Slope, rising from x0 to the ridge over x1.
    buf.quad([x0, 0.0, z0], [x0, 0.0, z1], [x1, 1.0, z1], [x1, 1.0, z0]);
    // Glazed vertical face at x1.
    buf.quad([x1, 0.0, z1], [x1, 0.0, z0], [x1, 1.0, z0], [x1, 1.0, z1]);
    // End triangles, CCW seen from outside (from -z and +z respectively).
    buf.tri([x0, 0.0, z0], [x1, 1.0, z0], [x1, 0.0, z0]);
    buf.tri([x1, 0.0, z1], [x1, 1.0, z1], [x0, 0.0, z1]);
    // Underside, for the same reason as the gable's soffit: a wedge with no
    // bottom is a hole you can see the sky through from under the wall.
    buf.quad([x0, 0.0, z1], [x0, 0.0, z0], [x1, 0.0, z0], [x1, 0.0, z1]);

    buf.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every face must point away from the mesh: one inverted triangle shows up
    /// in the game as a hole in the skyline, and nothing else in this module can
    /// catch it.
    #[test]
    fn every_triangle_faces_outward() {
        for (name, mesh) in [("gable", bake_gable()), ("tooth", bake_tooth())] {
            let positions: Vec<[f32; 3]> = mesh
                .attribute(Mesh::ATTRIBUTE_POSITION)
                .expect("positions")
                .as_float3()
                .expect("f32")
                .to_vec();
            let indices = match mesh.indices().expect("indices") {
                Indices::U32(i) => i.clone(),
                Indices::U16(i) => i.iter().map(|&v| v as u32).collect(),
            };
            let centroid: [f32; 3] = (0..3)
                .map(|axis| positions.iter().map(|p| p[axis]).sum::<f32>() / positions.len() as f32)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();

            #[allow(clippy::chunks_exact_to_as_chunks)] // Indices is an enum, not a slice
            for tri in indices.as_chunks::<3>().0 {
                let (a, b, c) = (
                    positions[tri[0] as usize],
                    positions[tri[1] as usize],
                    positions[tri[2] as usize],
                );
                let n = face_normal(a, b, c);
                let mid = [
                    (a[0] + b[0] + c[0]) / 3.0 - centroid[0],
                    (a[1] + b[1] + c[1]) / 3.0 - centroid[1],
                    (a[2] + b[2] + c[2]) / 3.0 - centroid[2],
                ];
                let dot = n[0] * mid[0] + n[1] * mid[1] + n[2] * mid[2];
                assert!(
                    dot > 1e-3,
                    "{name}: a triangle's normal points inward (dot {dot:.4}) at {a:?} {b:?} {c:?}"
                );
            }
        }
    }

    /// Every roof mesh is a closed shell: a directed edge must have exactly
    /// one partner running the other way. One missing face is a hole you look
    /// the sky through -- the gable's soffit was missing, and from under the
    /// overhang the only thing between the walker and the sky was a back face
    /// the rasteriser culls.
    #[test]
    fn roofs_are_closed_shells() {
        for (name, mesh) in [("gable", bake_gable()), ("tooth", bake_tooth())] {
            let positions: Vec<[f32; 3]> = mesh
                .attribute(Mesh::ATTRIBUTE_POSITION)
                .expect("positions")
                .as_float3()
                .expect("f32")
                .to_vec();
            let indices = match mesh.indices().expect("indices") {
                Indices::U32(i) => i.clone(),
                Indices::U16(i) => i.iter().map(|&v| v as u32).collect(),
            };
            // Quantise so vertices shared between faces compare equal.
            let key = |p: [f32; 3]| p.map(|v| (v * 1024.0).round() as i64);
            let mut edges: std::collections::HashMap<([i64; 3], [i64; 3]), u32> =
                std::collections::HashMap::new();
            for tri in indices.as_chunks::<3>().0 {
                let k = [
                    key(positions[tri[0] as usize]),
                    key(positions[tri[1] as usize]),
                    key(positions[tri[2] as usize]),
                ];
                for i in 0..3 {
                    *edges.entry((k[i], k[(i + 1) % 3])).or_default() += 1;
                }
            }
            for (edge, count) in &edges {
                assert_eq!(*count, 1, "{name}: edge {edge:?} is used {count} times");
                let reverse = edges.get(&(edge.1, edge.0)).copied().unwrap_or(0);
                assert_eq!(
                    reverse, 1,
                    "{name}: open edge {:?} -> {:?} (partner found {reverse} times): the shell has a hole",
                    edge.0, edge.1
                );
            }
        }
    }

    /// The gable's ridge runs along x at the top, and the eaves overhang the
    /// depth by the baked fraction on both sides.
    #[test]
    fn gable_ridge_runs_along_the_frontage() {
        let mesh = bake_gable();
        let positions: Vec<[f32; 3]> = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap()
            .to_vec();
        // Top vertices: the ridge, and only the ridge, reaches y = 1.
        let ridge: Vec<[f32; 3]> = positions
            .iter()
            .filter(|p| (p[1] - 1.0).abs() < 1e-6)
            .copied()
            .collect();
        assert!(!ridge.is_empty(), "no ridge vertex reaches y = 1");
        for p in &ridge {
            assert!(p[2].abs() < 1e-6, "ridge vertex off the centreline: {p:?}");
        }
        let extremes = (
            positions.iter().map(|p| p[2]).fold(f32::INFINITY, f32::min),
            positions
                .iter()
                .map(|p| p[2])
                .fold(f32::NEG_INFINITY, f32::max),
        );
        assert!((extremes.0 - -0.56).abs() < 1e-5, "eave {extremes:?}");
        assert!((extremes.1 - 0.56).abs() < 1e-5, "eave {extremes:?}");
    }

    /// The gable-end caps must span the slope's full width. The slope edges run
    /// out to the eave (|z| = 0.56) while the wall below is only 0.5 wide; a cap
    /// built to the wall line leaves a thin triangular slit of sky between roof
    /// and gable at each end, which reads as a hole in the skyline.
    #[test]
    fn gable_end_caps_reach_the_eave() {
        let mesh = bake_gable();
        let positions: Vec<[f32; 3]> = mesh
            .attribute(Mesh::ATTRIBUTE_POSITION)
            .unwrap()
            .as_float3()
            .unwrap()
            .to_vec();
        let indices = match mesh.indices().expect("indices") {
            Indices::U32(i) => i.clone(),
            Indices::U16(i) => i.iter().map(|&v| v as u32).collect(),
        };
        let mut caps = 0;
        for tri in indices.as_chunks::<3>().0 {
            let pts = [
                positions[tri[0] as usize],
                positions[tri[1] as usize],
                positions[tri[2] as usize],
            ];
            // A cap is the only triangle whose three vertices share one x end.
            let at_x_end = pts.iter().all(|p| (p[0] - 0.5).abs() < 1e-6)
                || pts.iter().all(|p| (p[0] + 0.5).abs() < 1e-6);
            if !at_x_end {
                continue;
            }
            caps += 1;
            let base: Vec<_> = pts.iter().filter(|p| p[1].abs() < 1e-6).collect();
            assert_eq!(base.len(), 2, "end cap is not a wall tri: {pts:?}");
            for p in base {
                assert!(
                    (p[2].abs() - 0.56).abs() < 1e-5,
                    "gable end cap stops short of the eave: {p:?}"
                );
            }
        }
        assert_eq!(caps, 2, "gable has {caps} end caps, expected one per end");
    }

    /// Teeth repeat along the frontage and tile it exactly: the count is the
    /// nearest whole pitch and the width absorbs the remainder.
    #[test]
    fn sawtooth_teeth_tile_the_frontage() {
        let profile = RoofProfile::of(FacadeStyle::Factory);
        for frontage in [26.0, 42.0, 47.5, 54.0] {
            let (count, width) = profile.teeth(frontage);
            assert!(count >= 1);
            assert!(
                (width * count as f32 - frontage).abs() < 1e-4,
                "teeth {count} x {width:.2} do not tile {frontage:.1} m"
            );
            assert!(
                (width - SAWTOOTH_PITCH).abs() < 1.5,
                "tooth width {width:.2} m drifted a quarter pitch from {SAWTOOTH_PITCH}"
            );
        }
        // A short frontage still gets one tooth, never zero or negative.
        let (count, width) = profile.teeth(3.0);
        assert_eq!(count, 1);
        assert!((width - 3.0).abs() < 1e-5);
    }

    /// Rises stay in the silhouette budget: tall enough to cut the sky, short
    /// enough that a roof is not a second building.
    #[test]
    fn rises_stay_inside_the_silhouette_budget() {
        for style in FacadeStyle::ALL {
            let profile = RoofProfile::of(style);
            for variant in 0..2 {
                let rise = profile.rise_for(variant);
                assert!(
                    (0.4..=2.6).contains(&rise),
                    "{}: rise {rise:.2} m is outside the roof budget",
                    style.label()
                );
            }
        }
    }

    /// A roof is darker than the wall it sits on. The read at dusk is a dark
    /// mass against a bright sky; matching the wall would merge the building
    /// into its own top.
    #[test]
    fn tint_is_darker_than_its_wall() {
        // Wall mean albedo, measured by `world::facade`'s stats (sRGB).
        for style in FacadeStyle::ALL {
            let profile = RoofProfile::of(style);
            let luma =
                0.2126 * profile.tint[0] + 0.7152 * profile.tint[1] + 0.0722 * profile.tint[2];
            let limit = match style {
                FacadeStyle::OldCity => 0.54,
                FacadeStyle::Apartment => 0.47,
                FacadeStyle::Factory => 0.27,
            };
            assert!(
                luma < limit,
                "{}: roof luma {luma:.3} is not darker than its wall ({limit:.3})",
                style.label()
            );
        }
    }
}
