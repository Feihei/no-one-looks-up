//! Structured reflection environment (B).
//!
//! # Why the glass reflected nothing
//!
//! The facade material has reflected the sky since R3.11: the fragment stage is
//! Bevy's PBR shader, the metal/rough map makes glass a smooth dielectric, and
//! `bevy_elements` puts an `AtmosphereEnvironmentMapLight` on the camera. The
//! reflection was *physically* there, but visually absent, because the probe it
//! samples is a perfectly smooth atmosphere gradient. A mirror of a gradient is
//! a flat wash of colour, and the eye needs *structure* to read it as a mirror.
//!
//! The project's own clouds make it worse rather than better: the cumulus pass
//! is a fullscreen composite and the cirrus dome is a separate mesh, so neither
//! is ever rendered into the atmosphere probe. The mirrored sky contains no
//! cloud at all.
//!
//! # What this adds
//!
//! A `LightProbe` carrying a small procedurally baked cubemap of what a
//! street-level window actually faces: a dusk sky with a bright horizon band
//! and a warm sun lobe, a thin high veil, and — the important part — the
//! buildings on the far side of the street, whose roofline wanders across the
//! whole horizon and drops to near-black below. That hard edge and the dark
//! lower half are the structure the eye reads as "a reflection", and the
//! atmosphere probe has neither.
//!
//! The cube is filtered to IBL by `GeneratedEnvironmentMapLight`, the same path
//! the cloud shader already uses for its ambient term, so no custom render pass
//! is involved.
//!
//! Two honest caveats. The game's sky is mostly clear by default (a thin
//! stratocumulus deck at 0.35 amount, faint cirrus at 0.06), so the veil in the
//! probe is deliberately low-contrast: it breaks the gradient into bands
//! rather than inventing weather. And the roofline is a
//! generic skyline, not the real one, because the probe is baked once at
//! startup and the map is data-driven; a real per-location capture would need a
//! runtime cube render, which is a much larger piece of work.
//!
//! The probe covers the whole map, which means it *replaces* the atmosphere
//! probe inside the city (light probes accumulate by weight and the camera map
//! fills the remainder). `DuskSky::reflection_intensity` is therefore both the
//! tuning knob and the off switch: at 0 the probe is hidden and the atmosphere
//! takes over exactly as before. F5 → "reflection" compares the two live.
//!
//! # Why not SSR
//!
//! Bevy 0.20 has `ScreenSpaceReflections`, but it requires deferred rendering,
//! which conflicts with the project's `Msaa::Off` + depth-prepass cloud
//! composite, and it can only reflect what is on screen. A window opposite a
//! wall has nothing useful on screen to reflect. This probe costs one 64²
//! cubemap baked once and one convolution pass at startup.

use bevy::asset::RenderAssetUsages;
use bevy::image::Image;
use bevy::light::{GeneratedEnvironmentMapLight, LightProbe};
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDataOrder, TextureDimension, TextureFormat, TextureViewDescriptor,
    TextureViewDimension,
};

use crate::plugins::sky::DuskSky;
use crate::world::world_center;

/// Cubemap face size. 64² is enough: the cubemap is convolved into roughness
/// mips before it is ever sampled, so detail above a few degrees per texel
/// would be blurred away. Keeping it small also keeps the startup bake to a
/// few milliseconds.
pub const CUBE_SIZE: u32 = 64;

/// Probe box half-size (m). The map is 800 m across, so 900 m covers it with
/// margin and the camera is always inside the probe.
const PROBE_HALF_EXTENT: f32 = 900.0;

/// Probe box half-height (m). Buildings top out around 48 m, so 120 m clears
/// the tallest roof and the probe still never leaves the camera outside.
const PROBE_HALF_HEIGHT: f32 = 120.0;

/// Direction of the golden-hour sun, in world space. The sky is frozen
/// (ADR-0001), so a constant is honest here; if the date or hour ever becomes
/// tunable this must follow it.
const SUN_DIR: Vec3 = Vec3::new(-0.42, 0.055, -0.90);

pub struct ReflectionProbePlugin;

impl Plugin for ReflectionProbePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_probe)
            .add_systems(Update, sync_probe);
    }
}

/// Spawns the one probe that covers the walkable map.
fn spawn_probe(mut commands: Commands, mut images: ResMut<Assets<Image>>, sky: Res<DuskSky>) {
    let cube = images.add(bake_sky_cube());
    commands.spawn((
        Name::new("ReflectionProbe"),
        LightProbe::default(),
        GeneratedEnvironmentMapLight {
            environment_map: cube,
            intensity: sky.reflection_intensity,
            rotation: Quat::IDENTITY,
            affects_lightmapped_mesh_diffuse: true,
        },
        Transform::from_xyz(world_center(), PROBE_HALF_HEIGHT, world_center()).with_scale(
            Vec3::new(
                PROBE_HALF_EXTENT * 2.0,
                PROBE_HALF_HEIGHT * 2.0,
                PROBE_HALF_EXTENT * 2.0,
            ),
        ),
    ));
    info!(
        "reflection probe spawned: {CUBE_SIZE}^2 cube, intensity {}",
        sky.reflection_intensity
    );
}

/// Applies the panel's reflection strength to both components.
///
/// `GeneratedEnvironmentMapLight` is what the filter reads; the
/// `EnvironmentMapLight` it inserts is what the renderer samples. Tuning only
/// the first would look like a dead knob until the next restart, so both move
/// together. At 0 the probe is hidden, which hands the view back to the
/// atmosphere probe — the A/B comparison.
fn sync_probe(
    sky: Res<DuskSky>,
    mut probes: Query<
        (
            &mut GeneratedEnvironmentMapLight,
            Option<&mut EnvironmentMapLight>,
            &mut Visibility,
        ),
        With<LightProbe>,
    >,
) {
    if !sky.is_changed() {
        return;
    }
    let intensity = sky.reflection_intensity;
    let visibility = if intensity > 0.0 {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    for (mut generated, sampled, mut visibility_slot) in &mut probes {
        if generated.intensity != intensity {
            generated.intensity = intensity;
        }
        if let Some(mut sampled) = sampled
            && sampled.intensity != intensity
        {
            sampled.intensity = intensity;
        }
        if *visibility_slot != visibility {
            *visibility_slot = visibility;
        }
    }
}

/// Bakes the source cubemap: dusk gradient + sun lobe + cloud banks + a dark
/// ground band.
///
/// Stored as linear `Rgba32Float` radiance. Values are O(0.1..3): the intensity
/// on the component (cd/m²) is what places the result in the scene's exposure,
/// not these numbers.
fn bake_sky_cube() -> Image {
    let size = CUBE_SIZE;
    let mut data = Vec::with_capacity((size * size * 6 * 16) as usize);
    for face in 0..6 {
        for y in 0..size {
            for x in 0..size {
                let u = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                // Cubemap rows run top-down in the image, so v = -1 is up.
                let v = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
                let dir = face_direction(face, u, v);
                let radiance = sky_radiance(dir);
                for channel in radiance {
                    data.extend_from_slice(&channel.to_le_bytes());
                }
                // Alpha is unused by the filter but the format is RGBA.
                data.extend_from_slice(&1.0f32.to_le_bytes());
            }
        }
    }

    let mut image = Image::new_uninit(
        Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 6,
        },
        // A cubemap is a 2D texture with six array layers plus a cube *view*;
        // `TextureDimension` itself only has D1/D2/D3. Bevy's own
        // `hemispherical_gradient_cubemap` is built the same way.
        TextureDimension::D2,
        TextureFormat::Rgba32Float,
        RenderAssetUsages::default(),
    );
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::Cube),
        ..default()
    });
    image.data = Some(data);
    image.data_order = TextureDataOrder::LayerMajor;
    image
}

/// Direction for a cubemap texel.
///
/// The face order and the `-v` mapping are the standard cubemap convention
/// (WebGPU/D3D): +X, -X, +Y, -Y, +Z, -Z, with `u` right and `v` down.
fn face_direction(face: u32, u: f32, v: f32) -> Vec3 {
    let dir = match face {
        0 => Vec3::new(1.0, -v, -u),
        1 => Vec3::new(-1.0, -v, u),
        2 => Vec3::new(u, 1.0, v),
        3 => Vec3::new(u, -1.0, -v),
        4 => Vec3::new(u, -v, 1.0),
        _ => Vec3::new(-u, -v, -1.0),
    };
    dir.normalize()
}

/// Linear radiance of the dusk street in one direction.
///
/// Three things, in order: the sky gradient with the sun lobe, thin high cloud
/// above it, then the buildings on the far side of the street. The buildings
/// are what a street-level window mostly reflects, and their roofline is the
/// one hard horizontal edge in the whole image — without it the mirror is a
/// gradient again, just a warmer one.
///
/// Every noise lookup is sampled on a *circle* in the azimuth, not on `atan2`
/// directly. `atan2` jumps at ±π, and an azimuth-linear noise would put a
/// visible vertical seam in every window that faces that way.
fn sky_radiance(dir: Vec3) -> [f32; 3] {
    let horizon = Vec3::new(1.35, 0.52, 0.20);
    let mid = Vec3::new(0.62, 0.34, 0.30);
    let zenith = Vec3::new(0.09, 0.12, 0.27);

    let height = dir.y.clamp(0.0, 1.0);
    let t = (height / 0.75).powf(0.55);
    let mut colour = if t < 0.5 {
        horizon.lerp(mid, t * 2.0)
    } else {
        mid.lerp(zenith, (t - 0.5) * 2.0)
    };

    // The sun lobe: a tight disc plus a broad forward-scattering skirt. This is
    // the bright smear a window actually reflects at golden hour.
    let sun = dir.dot(SUN_DIR.normalize()).clamp(0.0, 1.0);
    colour += Vec3::new(2.4, 1.05, 0.35) * (0.75 * sun.powf(180.0) + 0.20 * sun.powf(6.0));

    let azimuth = dir.x.atan2(dir.z);
    let (ring_x, ring_y) = (azimuth.cos(), azimuth.sin());

    // Thin high cloud: a broad veil rather than distinct puffs. The game's own
    // sky is mostly clear by default (low-cover stratocumulus, faint cirrus),
    // so the probe shows this at low contrast — it is here to break the
    // gradient into bands, not to invent weather the sky above does not have.
    let cloud_noise = fbm(
        ring_x * 1.5 + 4.0,
        ring_y * 1.5 + 4.0 + height * 2.2,
        0x51ed,
    );
    let cover = smoothstep(0.50, 0.70, cloud_noise)
        * smoothstep(0.0, 0.16, height)
        * (1.0 - 0.45 * smoothstep(0.50, 0.95, height));
    let lit = 0.55 + 0.45 * sun;
    let cloud = Vec3::new(0.86, 0.53, 0.41) * lit;
    colour = colour.lerp(cloud, cover * 0.55);

    // The far side of the street. `roofline` wanders between roughly 1° and
    // 17° above the reflected horizon: across a narrow street the opposite
    // frontage subtends a lot of sky, so most of what a window mirrors is wall,
    // with the bright band only above the rooftops. That asymmetry is the whole
    // look — a horizon sitting at 0° would read as a wide avenue, not a street
    // canyon.
    //
    // The noise is stretched about its midpoint before it is used. Four-octave
    // value noise only spans about 0.25..0.65 in practice, which would put every
    // roof within a few degrees of every other and turn the skyline into a thin
    // stripe instead of a skyline.
    let skyline =
        ((fbm(ring_x * 2.3 + 9.0, ring_y * 2.3 + 9.0, 0x9e37) - 0.45) * 2.6).clamp(-1.0, 1.0);
    let roofline = 0.16 + 0.14 * skyline;
    let building = 1.0 - smoothstep(roofline - 0.012, roofline + 0.012, dir.y);
    let facade_detail = fbm(ring_x * 5.2 + 2.0, ring_y * 5.2 + 2.0, 0x2f1d);
    // 0 at street level, 1 just under the roofline: the top storeys still catch
    // the west sky, the shopfronts do not.
    let near_top = smoothstep(-0.40, roofline, dir.y);
    let facade =
        Vec3::new(0.040, 0.034, 0.030) * (0.55 + 0.45 * facade_detail) * (0.45 + 0.55 * near_top)
            + Vec3::new(0.090, 0.036, 0.012) * (0.25 * near_top * near_top * near_top);
    colour = colour.lerp(facade, building);
    [colour.x, colour.y, colour.z]
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn hash01(x: i32, y: i32, seed: u32) -> f32 {
    let mut h = (x as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add((y as u32).wrapping_mul(668_265_263))
        .wrapping_add(seed.wrapping_mul(2_246_822_519));
    h ^= h >> 13;
    h = h.wrapping_mul(1_274_126_177);
    h ^= h >> 16;
    (h & 0x00ff_ffff) as f32 / 16_777_216.0
}

fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let ix = x.floor();
    let iy = y.floor();
    let fx = smoothstep(0.0, 1.0, x - ix);
    let fy = smoothstep(0.0, 1.0, y - iy);
    let i0 = ix as i32;
    let j0 = iy as i32;
    let a = hash01(i0, j0, seed);
    let b = hash01(i0 + 1, j0, seed);
    let c = hash01(i0, j0 + 1, seed);
    let d = hash01(i0 + 1, j0 + 1, seed);
    let top = a + (b - a) * fx;
    let bottom = c + (d - c) * fx;
    top + (bottom - top) * fy
}

fn fbm(x: f32, y: f32, seed: u32) -> f32 {
    let mut sum = 0.0;
    let mut amplitude = 1.0;
    let mut total = 0.0;
    let mut frequency = 1.0;
    for octave in 0..4 {
        sum += value_noise(x * frequency, y * frequency, seed + octave * 977) * amplitude;
        total += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    sum / total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_far_side_of_the_street_breaks_the_horizon() {
        // A gradient — exactly what the atmosphere probe gives — is uniform
        // across azimuth at every elevation, and a uniform mirror is the flat
        // wash this module exists to fix. The roofline is what breaks it, so
        // some ring of the sky has to contain both open sky and dark frontage.
        // Which ring depends on the roofline heights, so search a range instead
        // of hard-coding one elevation.
        let mut mixed_rings = 0;
        for elevation in [0.06f32, 0.10, 0.16, 0.24, 0.34, 0.50] {
            let scale = (1.0 - elevation * elevation).sqrt();
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            let (mut dark, mut bright) = (0, 0);
            for i in 0..512 {
                let azimuth = i as f32 / 512.0 * std::f32::consts::TAU;
                let dir =
                    Vec3::new(azimuth.cos() * scale, elevation, azimuth.sin() * scale).normalize();
                let l = luminance(sky_radiance(dir));
                lo = lo.min(l);
                hi = hi.max(l);
                if l < 0.10 {
                    dark += 1;
                } else if l > 0.50 {
                    bright += 1;
                }
            }
            if dark > 20 && bright > 20 && hi > lo * 5.0 {
                mixed_rings += 1;
            }
        }
        assert!(
            mixed_rings >= 2,
            "no elevation crosses a roofline: the reflection is a gradient at every height"
        );
    }

    #[test]
    fn the_reflection_has_no_seam_where_the_azimuth_wraps() {
        // `atan2` jumps at ±π. Sampling the noise on a circle keeps the two
        // sides of the seam equal; a linear-in-azimuth noise would show a
        // vertical line in every window facing that way.
        let eps = 1e-4;
        for elevation in [0.02f32, 0.15, 0.45, 0.85] {
            let scale = (1.0 - elevation * elevation).sqrt();
            let a = sky_radiance(
                Vec3::new(
                    (std::f32::consts::PI - eps).cos() * scale,
                    elevation,
                    (std::f32::consts::PI - eps).sin() * scale,
                )
                .normalize(),
            );
            let b = sky_radiance(
                Vec3::new(
                    (-std::f32::consts::PI + eps).cos() * scale,
                    elevation,
                    (-std::f32::consts::PI + eps).sin() * scale,
                )
                .normalize(),
            );
            let difference = (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs();
            assert!(
                difference < 1e-2,
                "seam at {elevation} elevation: the two sides differ by {difference:.4}"
            );
        }
    }

    #[test]
    fn the_cube_is_linear_radiance_with_a_dark_ground_and_a_bright_horizon() {
        let up = sky_radiance(Vec3::Y);
        // 21° up: clear of the tallest roofline (about 18°), so this is sky.
        // The literal horizon is behind the opposite buildings by design, which
        // is why this compares the low sky rather than 0°.
        let low_sky = sky_radiance(Vec3::new(1.0, 0.36, 0.0).normalize());
        let down = sky_radiance(Vec3::NEG_Y);
        assert!(
            luminance(low_sky) > luminance(up) * 1.3,
            "the low sky must be the bright band the glass reflects ({} vs {})",
            luminance(low_sky),
            luminance(up)
        );
        assert!(
            luminance(down) < 0.05,
            "the street must be dark, got {}",
            luminance(down)
        );
    }

    #[test]
    fn every_face_is_covered_and_normalised() {
        for face in 0..6 {
            for (u, v) in [(-0.95, -0.95), (0.0, 0.0), (0.95, 0.95)] {
                let dir = face_direction(face, u, v);
                assert!(
                    (dir.length() - 1.0).abs() < 1e-4,
                    "face {face} direction is not normalised"
                );
            }
        }
    }

    #[test]
    fn the_cube_has_six_power_of_two_faces() {
        let image = bake_sky_cube();
        // The texture itself is a 6-layer 2D array; the cube is the *view*.
        // If the view were left as D2 the filter would read a flat atlas and
        // the reflection would be garbage, so this is worth asserting.
        assert_eq!(image.texture_descriptor.dimension, TextureDimension::D2);
        assert_eq!(
            image
                .texture_view_descriptor
                .as_ref()
                .and_then(|view| view.dimension),
            Some(TextureViewDimension::Cube)
        );
        assert_eq!(image.texture_descriptor.size.width, CUBE_SIZE);
        assert_eq!(image.texture_descriptor.size.depth_or_array_layers, 6);
        assert!(CUBE_SIZE.is_power_of_two());
        assert_eq!(
            image.data.as_ref().expect("bake must fill the image").len(),
            (CUBE_SIZE * CUBE_SIZE * 6 * 16) as usize
        );
    }

    fn luminance(c: [f32; 3]) -> f32 {
        c[0].max(c[1]).max(c[2])
    }
}
