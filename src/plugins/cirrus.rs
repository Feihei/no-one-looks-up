//! Cirrus: high, thin, wispy cloud rendered as a shaded sky dome (R5.6).
//!
//! Why not the volumetric layer: cirrus sits at 6-12 km and reads as a flat
//! sheet with essentially no parallax, so marching it as volume pays for depth
//! nobody can see. Its defining look is stretched fibre, which is exactly what
//! 2D noise stretched along one axis produces and what volumetric noise is
//! worst at. A dome shader is a fraction of the cost.
//!
//! Why a mesh rather than a full-screen pass: the dome goes through the normal
//! mesh pipeline, so the depth buffer occludes it behind buildings and trees
//! for free. A full-screen post-process pass cannot see depth (its render pass
//! has no depth attachment), and would paint cirrus over the street.
//!
//! The dome is a large inverted sphere that follows the camera. Its own
//! rotation is fixed, so the cloud pattern stays put in world space as you walk
//! (that is what makes it read as "high up" rather than "stuck to the lens").

use bevy::mesh::MeshBuilder;
use bevy::pbr::{Material, MaterialPlugin};
use bevy::prelude::*;
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::ShaderRef;

/// Radius of the dome (m).
///
/// Two hard bounds decide this:
/// - **Inside the camera's far plane.** `PerspectiveProjection::default()`
///   uses `far = 1000 m` and nothing in this project overrides it, so a larger
///   dome would be clipped away entirely and the sky would simply lose its
///   cirrus. (A 4 km dome was the first attempt and did exactly that.)
/// - **Outside all world geometry.** The streaming window is
///   `CHUNK_AHEAD * CHUNK_SIZE = 200 m` and trees are culled at 220 m, so 900 m
///   leaves the dome comfortably behind everything that can be drawn.
pub const DOME_RADIUS: f32 = 900.0;

/// Icosphere parameter for the dome.
///
/// **Beware the meaning**: this Bevy version's `SphereMeshBuilder::ico(n)`
/// produces `20 * (n + 1)^2` triangles, NOT the `20 * 4^n` of classic
/// subdivision -- `ico(3)` is 320 triangles, not 1280 (measured). 7 => 1280,
/// which keeps the silhouette smooth without being wasteful.
const DOME_SUBDIVISIONS: u32 = 7;

/// Cirrus look and placement, kept in one struct so the sky panel (R5.1) can
/// drive it and the values can be asserted without a GPU.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct CirrusSettings {
    /// How much cirrus the sky carries, 0..1. 0 disables the dome entirely.
    pub amount: f32,
    /// How far the fibre stretches, 1..8. High values give long streaks.
    pub stretch: f32,
    /// Noise frequency; larger is finer.
    pub scale: f32,
    /// Neighbouring streaks stay this similar, 0..1.
    pub directionality: f32,
    /// Drift speed (m/s) of the pattern, so the sky is not frozen.
    pub drift: f32,
}

impl Default for CirrusSettings {
    fn default() -> Self {
        Self {
            // 2026-11-24: Feihei's live F5 tuning. A faint, long, fine veil
            // reads better against the dusk than no cirrus at all; the
            // volumetric stratocumulus stays the main cloud.
            amount: 0.06,
            stretch: 1.4,
            scale: 3.6,
            directionality: 0.75,
            drift: 6.0,
        }
    }
}

/// Uniform sent to the dome shader. `AsBindGroup` packs it into binding 0.
#[derive(Clone, Copy, Debug, ShaderType)]
pub struct CirrusUniform {
    /// xyz: unit direction toward the sun. w: amount.
    pub sun: Vec4,
    /// rgb: sky-scattered light tint (linear). a: brightness.
    pub tint: Vec4,
    /// x: stretch. y: scale. z: directionality. w: drift time (s).
    pub params: Vec4,
}

impl Default for CirrusUniform {
    fn default() -> Self {
        Self {
            sun: Vec4::new(0.0, 0.1, -1.0, 0.0),
            tint: Vec4::new(1.0, 0.85, 0.7, 1.0),
            // Keep the material's first frame in step with `CirrusSettings`;
            // later frames overwrite these from the live settings anyway.
            params: Vec4::new(1.4, 3.6, 0.75, 0.0),
        }
    }
}

/// Material for the cirrus dome.
#[derive(Asset, AsBindGroup, TypePath, Clone, Debug, Default)]
pub struct CirrusMaterial {
    #[uniform(0)]
    pub uniform: CirrusUniform,
}

impl Material for CirrusMaterial {
    fn vertex_shader() -> ShaderRef {
        "shaders/cirrus.wesl".into()
    }

    fn fragment_shader() -> ShaderRef {
        "shaders/cirrus.wesl".into()
    }

    /// Cirrus is drawn after the opaque scene so its few fragments blend over
    /// the sky; it writes no depth.
    fn alpha_mode(&self) -> AlphaMode {
        AlphaMode::Blend
    }

    /// The dome does not receive light or cast shadows: its shading is the
    /// analytic sky tint computed in the fragment shader.
    fn enable_shadows() -> bool {
        false
    }

    /// The camera is INSIDE the dome, so every triangle is back-facing. Bevy's
    /// mesh pipeline culls back faces by default for every alpha mode, which
    /// makes a plain sphere invisible from within. Handing the shader both
    /// faces is simpler and more reliable than mirroring the geometry: a
    /// negative scale does not actually reverse winding once the triangle has
    /// moved to the other side (verified numerically).
    fn reads_view_transmission_texture(&self) -> bool {
        false
    }
}

/// The dome entity, kept as a component so the follow system can find it.
#[derive(Component)]
pub struct CirrusDome;

pub struct CirrusPlugin;

impl Plugin for CirrusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CirrusSettings>()
            .add_plugins(MaterialPlugin::<CirrusMaterial>::default())
            .add_systems(Startup, spawn_dome)
            .add_systems(Update, (follow_camera, update_uniform));
    }
}

fn spawn_dome(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<CirrusMaterial>>,
) {
    info!("cirrus dome spawned: radius {DOME_RADIUS} m, amount 0 = hidden");
    commands.spawn((
        Name::new("CirrusDome"),
        CirrusDome,
        Mesh3d(meshes.add(dome_sphere(DOME_RADIUS))),
        MeshMaterial3d(materials.add(CirrusMaterial::default())),
        // No scale flip. A negative scale mirrors the geometry, which does
        // NOT reliably reverse triangle winding (verified: the cross product
        // of a mirrored triangle keeps its sign once the triangle has moved to
        // the other side). The dome is seen from inside, so the shader emits
        // both faces instead -- see the `cull_mode: None` note on the material.
        Transform::IDENTITY,
        // Never culled: the camera is always inside it.
        Visibility::default(),
        bevy::camera::visibility::NoFrustumCulling,
    ));
}

/// An **icosphere** at `radius`, wound inside-out.
///
/// Icosphere, not UV sphere: a UV sphere's poles pinch into slivers (measured
/// on the default 32x64 sphere: the first polar ring's triangles have an
/// aspect ratio of 0.098, i.e. long thin needles). Rasterising those produces
/// interpolation artefacts that read as radiation-stripes converging on the
/// pole -- which is what a "seam" looked like in the sky. An icosphere's
/// triangles are near-equilateral everywhere and it has no poles.
///
/// Inside-out winding: the camera sits at the centre and looks at the inner
/// surface, so every triangle would be back-facing and culled. Bevy's
/// `Material` trait has no `cull_mode` hook, so the fix has to be in the
/// geometry: swap each triangle's last two indices and the same faces become
/// front-facing from within.
fn dome_sphere(radius: f32) -> Mesh {
    let mut mesh = Sphere::new(radius)
        .mesh()
        .ico(DOME_SUBDIVISIONS)
        // 3 subdivisions is ~1.3 k triangles: smooth enough that no triangle
        // edge is visible in a soft noise field, cheap enough to ignore.
        .unwrap_or_else(|_| Sphere::new(radius).mesh().build());
    match mesh.indices_mut() {
        Some(bevy::mesh::Indices::U32(indices)) => {
            for triangle in indices.as_chunks_mut::<3>().0 {
                triangle.swap(1, 2);
            }
        }
        Some(bevy::mesh::Indices::U16(indices)) => {
            for triangle in indices.as_chunks_mut::<3>().0 {
                triangle.swap(1, 2);
            }
        }
        None => {}
    }
    mesh
}

/// Keeps the dome centred on the camera. The dome does not inherit the
/// camera's rotation, so the pattern stays world-aligned.
fn follow_camera(
    cameras: Query<&GlobalTransform, (With<Camera3d>, Without<CirrusDome>)>,
    mut domes: Query<&mut Transform, With<CirrusDome>>,
) {
    let Ok(camera) = cameras.single() else {
        return;
    };
    let centre = camera.translation();
    for mut transform in &mut domes {
        if transform.translation != centre {
            transform.translation = centre;
        }
    }
}

/// Copies the live sky tint and sun direction into the dome's uniform, and
/// hides the dome when cirrus is off.
///
/// The tint comes from the camera's `AmbientLight`, which bevy_elements derives
/// from the sky's own dusk gradient -- so cirrus does not need a second colour
/// model that could drift out of step with the rest of the sky.
fn update_uniform(
    settings: Res<CirrusSettings>,
    sun: Option<Res<bevy_elements::SunState>>,
    lights: Query<&AmbientLight, With<Camera3d>>,
    mut domes: Query<(&mut Visibility, &MeshMaterial3d<CirrusMaterial>), With<CirrusDome>>,
    mut materials: ResMut<Assets<CirrusMaterial>>,
    time: Res<Time>,
    mut elapsed: Local<f32>,
) {
    *elapsed += time.delta_secs();

    let amount = settings.amount.clamp(0.0, 1.0);
    let (sun_dir, altitude) = sun
        .as_ref()
        .map(|s| (s.direction.normalize_or(Vec3::Y), s.altitude_deg))
        .unwrap_or((Vec3::Y, 90.0));
    let light = lights.iter().next();
    let (tint, brightness) = match light {
        Some(l) => (l.color, l.brightness),
        None => (Color::WHITE, 1.0),
    };
    // Linear rgb of the ambient colour; the dome shades relative to it.
    let rgb = tint.to_linear();

    for (mut visibility, handle) in &mut domes {
        *visibility = if amount <= 0.001 {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
        if amount <= 0.001 {
            continue;
        }
        let Some(mut material) = materials.get_mut(&handle.0) else {
            continue;
        };
        material.uniform = CirrusUniform {
            sun: Vec4::new(sun_dir.x, sun_dir.y, sun_dir.z, amount),
            tint: Vec4::new(rgb.red, rgb.green, rgb.blue, brightness.max(0.0)),
            params: Vec4::new(
                settings.stretch.clamp(1.0, 8.0),
                settings.scale.clamp(0.2, 12.0),
                settings.directionality.clamp(0.0, 1.0),
                // Absolute time, not a per-frame delta: the pattern must not
                // jump when the amount changes or the frame rate varies.
                *elapsed * settings.drift,
            ),
        };
        let _ = altitude;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuned_cirrus_ships_on_and_off_is_still_expressible() {
        // 2026-11-24: Feihei's live F5 tuning, pinned so it does not drift.
        let settings = CirrusSettings::default();
        assert_eq!(settings.amount, 0.06);
        assert_eq!(settings.stretch, 1.4);
        assert_eq!(settings.scale, 3.6);

        // OFF must remain expressible: at 0 the dome hides rather than
        // rendering an invisible-but-present layer.
        let off = CirrusSettings {
            amount: 0.0,
            ..Default::default()
        };
        assert!(off.amount <= 0.001);
    }

    #[test]
    fn settings_stay_in_the_ranges_the_shader_assumes() {
        // The shader divides by `stretch` and uses `scale` as a frequency.
        // A zero stretch would divide by zero; a zero scale would collapse the
        // noise domain to a constant.
        let shader_clamp = |s: CirrusSettings| CirrusUniform {
            sun: Vec4::new(0.0, 1.0, 0.0, s.amount.clamp(0.0, 1.0)),
            tint: Vec4::ONE,
            params: Vec4::new(
                s.stretch.clamp(1.0, 8.0),
                s.scale.clamp(0.2, 12.0),
                s.directionality.clamp(0.0, 1.0),
                0.0,
            ),
        };
        for stretch in [-5.0f32, 0.0, 1.0, 8.0, 100.0] {
            for scale in [-1.0f32, 0.0, 0.2, 12.0, 999.0] {
                let u = shader_clamp(CirrusSettings {
                    stretch,
                    scale,
                    ..Default::default()
                });
                assert!(
                    u.params.x >= 1.0 && u.params.x <= 8.0,
                    "stretch {stretch} escaped its range: {}",
                    u.params.x
                );
                assert!(
                    u.params.y >= 0.2 && u.params.y <= 12.0,
                    "scale {scale} escaped its range: {}",
                    u.params.y
                );
            }
        }
    }

    #[test]
    fn the_dome_is_inside_the_far_plane_and_outside_the_world() {
        // Both bounds are real. The upper one bit immediately: the first dome
        // was 4 km, `PerspectiveProjection::default().far` is 1000 m, and
        // nothing in this project raises it -- so the dome was clipped away
        // and rendered nothing at all.
        // Read through a local so the constants stay comparable rather than
        // being folded into a constant `true`/`false` at compile time.
        let dome = DOME_RADIUS;
        let far = 1000.0f32;
        assert!(
            dome < far * 0.95,
            "dome radius {dome} m reaches the camera's default far plane              ({far} m) and would be clipped"
        );

        let streaming_extent = crate::world::CHUNK_AHEAD as f32 * crate::world::CHUNK_SIZE;
        assert!(
            DOME_RADIUS > streaming_extent * 1.5,
            "dome radius {DOME_RADIUS} m is too close to the {streaming_extent} m              streaming window; geometry would poke through it"
        );
        // Trees are culled at 220 m, handled by the bound above.
    }

    #[test]
    fn stretching_keeps_the_noise_domain_finite() {
        // The shader computes `1 - 1/stretch`; stretch is clamped to >= 1, so
        // that term stays in [0, 0.875]. Documented here because a future
        // change to the clamp would produce NaNs that discard every fragment.
        for stretch in [1.0f32, 2.0, 4.0, 8.0] {
            let factor = 1.0 - 1.0 / stretch.clamp(1.0, 8.0);
            assert!(
                factor.is_finite() && (0.0..=0.875).contains(&factor),
                "stretch {stretch} gives factor {factor}"
            );
        }
    }

    #[test]
    fn the_dome_has_no_polar_slivers() {
        // A UV sphere pinches at its poles into needles: on Bevy's default
        // 32x64 sphere the first polar ring's triangles measure an aspect
        // ratio of 0.098. Rasterising those produces interpolation artefacts
        // that read as seams radiating over the sky.
        //
        // An icosphere must stay near-equilateral everywhere. This walks every
        // triangle and bounds the worst aspect ratio, so a future switch back
        // to a UV sphere (or a coarser subdivision) fails loudly.
        let mesh = dome_sphere(DOME_RADIUS);
        let positions = mesh
            .attribute(bevy::mesh::Mesh::ATTRIBUTE_POSITION)
            .expect("positions");
        let Some(bevy::mesh::Indices::U32(indices)) = mesh.indices() else {
            panic!("expected u32 indices");
        };
        let pos = |i: u32| -> Vec3 {
            match positions {
                bevy::mesh::VertexAttributeValues::Float32x3(values) => {
                    Vec3::from_array(values[i as usize])
                }
                other => panic!("unexpected position format {other:?}"),
            }
        };

        let mut worst = f32::INFINITY;
        let mut counted = 0usize;
        for triangle in indices.as_chunks::<3>().0 {
            let (a, b, c) = (pos(triangle[0]), pos(triangle[1]), pos(triangle[2]));
            // Aspect = 4*sqrt(3)*area / (sum of squared edges): 1 for an
            // equilateral triangle, -> 0 as it degenerates to a needle.
            let area = (b - a).cross(c - a).length() * 0.5;
            let sum_sq =
                (b - a).length_squared() + (c - b).length_squared() + (a - c).length_squared();
            if sum_sq <= f32::EPSILON {
                continue;
            }
            let aspect = 4.0 * 3.0f32.sqrt() * area / sum_sq;
            worst = worst.min(aspect);
            counted += 1;
        }
        assert!(counted > 500, "only {counted} triangles");
        // 0.8 allows some distortion at the seam where the icosphere's base
        // icosahedron edges meet; a UV sphere sits at ~0.1.
        assert!(
            worst > 0.8,
            "worst triangle aspect ratio is {worst:.3}; the dome has slivers              (a UV sphere measures ~0.1) and will show radial seams"
        );
    }

    #[test]
    fn the_shader_flips_the_fragment_y_axis() {
        // `in.position.xy` is a fragment coordinate: origin top-left, y DOWN.
        // NDC is (-1,-1) bottom-left, (1,1) top-right, so y must be flipped.
        // Without the flip the sky is mirrored vertically and the pattern
        // appears to move the wrong way as the camera tilts -- the reported
        // "the texture follows the view".
        let shader = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shaders/cirrus.wesl"),
        )
        .expect("cirrus shader");
        assert!(
            shader.contains("1.0 - uv.y * 2.0"),
            "the fragment y axis is no longer flipped; NDC is y-up and the              fragment coordinate is y-down"
        );
        assert!(
            !shader.contains("(pixel / view.viewport.zw) * 2.0 - 1.0)"),
            "the unflipped NDC expression is back"
        );
    }

    #[test]
    fn the_shader_does_not_unproject_the_degenerate_far_plane() {
        // Bevy's projection is reverse-Z with an INFINITE far plane: depth 1.0
        // is the near plane, depth 0.0 is the point at infinity. Unprojecting
        // depth 0.0 divides by `w == 0`, giving NaN; a NaN normalises to NaN,
        // fails `dir.y < 0.02`, and every fragment is discarded -- cirrus
        // vanishes with amount at 1.0 and no error anywhere.
        //
        // Verified numerically against Bevy's projection: depth 0.0 yields
        // (nan, nan, -inf), depths 0.95 / 0.55 yield finite forward rays.
        let shader = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shaders/cirrus.wesl"),
        )
        .expect("cirrus shader");

        assert!(
            !shader.contains("vec4<f32>(ndc, 0.0, 1.0)"),
            "the shader unprojects depth 0.0 again, which is the point at              infinity: the direction becomes NaN and the cirrus disappears"
        );
        // Two finite depths must be present.
        assert!(
            shader.contains("ndc, 0.95, 1.0") && shader.contains("ndc, 0.55, 1.0"),
            "the shader no longer takes two finite depths for the ray"
        );
    }

    #[test]
    fn the_shader_rebuilds_the_direction_from_the_pixel() {
        // Interpolating `world_position` across 135 m triangles makes the
        // view direction deviate by up to 2.8e-3 rad and jump across every
        // triangle edge -- the seam. The fragment stage must unproject the
        // pixel instead; this pins that, because reverting to interpolation
        // is a one-line change that looks harmless.
        let shader = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/shaders/cirrus.wesl"),
        )
        .expect("cirrus shader");
        assert!(
            shader.contains("world_from_clip"),
            "the shader no longer unprojects the pixel; the view direction is              interpolated again and triangle seams will return"
        );
        assert!(
            !shader.contains("normalize(in.world_position.xyz - view.world_position.xyz)"),
            "the interpolated-direction expression is back"
        );
    }

    #[test]
    fn the_dome_geometry_faces_inward() {
        // The camera is inside the dome and Bevy culls back faces for every
        // alpha mode (`cull_mode: Some(Face::Back)`), so a normally-wound
        // sphere renders NOTHING from within. The mesh is wound inside-out;
        // this asserts that, because the failure mode is a completely
        // invisible sky with no error to point at.
        let mesh = dome_sphere(DOME_RADIUS);
        let positions = mesh
            .attribute(bevy::mesh::Mesh::ATTRIBUTE_POSITION)
            .expect("positions");
        let Some(bevy::mesh::Indices::U32(indices)) = mesh.indices() else {
            panic!("expected u32 indices");
        };
        let pos = |i: u32| -> Vec3 {
            match positions {
                bevy::mesh::VertexAttributeValues::Float32x3(values) => {
                    Vec3::from_array(values[i as usize])
                }
                other => panic!("unexpected position format {other:?}"),
            }
        };

        // For an inward-wound triangle, (b - a) x (c - a) points TOWARD the
        // centre, i.e. opposite the centroid direction.
        let mut checked = 0;
        for triangle in indices.as_chunks::<3>().0.iter().take(200) {
            let (a, b, c) = (pos(triangle[0]), pos(triangle[1]), pos(triangle[2]));
            let geometric = (b - a).cross(c - a);
            if geometric.length_squared() < 1e-9 {
                continue;
            }
            let centroid = (a + b + c) / 3.0;
            let outward = centroid.normalize();
            assert!(
                geometric.normalize().dot(outward) < 0.0,
                "triangle normal points outward; the dome would be culled                  from inside and the sky would show no cirrus"
            );
            checked += 1;
        }
        assert!(checked > 50, "only {checked} triangles were checked");
    }
}
