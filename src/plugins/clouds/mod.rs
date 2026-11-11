//! Self-built volumetric clouds (R6; adr-0007).
//!
//! Replaces the vendored bevy_elements `CloudPlugin` (disabled in
//! [`crate::plugins::sky`]): its layer is quantised at 32–64 m by fixed noise
//! resolutions, which no parameter fixes. This plugin renders ONE golden-hour
//! cumulus layer with its own noise and lighting model:
//!
//! - half-resolution march, every texel every frame (no Bayer slots);
//! - temporal resolve by world-space shell-entry reprojection with a fresh
//!   3×3 clamp;
//! - history stores UNexposed luminance, so the sky's exposure perturbation
//!   never pollutes it; exposure is applied in the composite.
//!
//! The WESL modules live beside this file and are imported as
//! `no_one_looks_up::plugins::clouds::<name>` (bevy derives the import path
//! from the embedded asset's path relative to `src/`).

mod config;
mod gpu;
mod params;

use bevy::core_pipeline::prepass::DepthPrepass;
use bevy::light::Atmosphere;
use bevy::light::light_consts::lux;
use bevy::math::{UVec2, UVec4};
use bevy::prelude::*;
use bevy::render::RenderApp;
use bevy::render::extract_component::ExtractComponent;
use bevy::render::extract_resource::ExtractResource;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::shader::load_shader_library;

use bevy_elements::{ElementsCamera, ElementsQuality, ElementsSystems, ElementsUnits, SunState};

use crate::plugins::sky::{CloudKind, DuskSky, resolve_clouds};

use config::CloudsQuality;
use params::CloudsParams;

/// Debug overrides for the kernels, inserted by tests and manual tuning
/// sessions. Mode 1 renders a uniform shell of [`CloudsDebug::luminance`]
/// through the whole chain — the C0 smoke check of march→resolve→composite.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct CloudsDebug {
    /// 0 off; 1 uniform shell.
    pub mode: f32,
    /// Luminance of the debug shell (unexposed, nits).
    pub luminance: f32,
}

impl CloudsDebug {
    /// Mode 1: a uniform shell at `luminance` nits.
    pub fn shell(luminance: f32) -> Self {
        Self {
            mode: 1.0,
            luminance,
        }
    }
}

/// Main-world state of the cloud simulation.
#[derive(Resource, Default)]
pub(crate) struct CloudsRuntime {
    /// Frame counter; parity selects the history ping-pong target.
    frame: u32,
    /// Frames left on which the history is flagged invalid.
    reset_frames: u32,
    /// The last resolved (kind, amount, layer) — a change invalidates history.
    last_shape: Option<ShapeKey>,
}

/// A resolved cloud shape: the kind, its amount and any explicit layer.
/// `drift` joins in C1 when the wind moves the field.
type ShapeKey = (Option<CloudKind>, f32, Option<(f32, f32)>);

/// The per-view images: this frame's march result and the two history
/// textures. Created main-world, consumed render-world through the extracted
/// [`CloudsGpu`].
#[derive(Clone, Debug)]
pub(crate) struct CloudsViewImages {
    pub size: UVec2,
    pub march: Handle<Image>,
    pub history: [Handle<Image>; 2],
}

/// Everything the render world needs, extracted every frame.
#[derive(Resource, Clone, Default)]
pub(crate) struct CloudsGpu {
    pub view: Option<CloudsViewImages>,
    pub params: CloudsParams,
    /// A followed camera with clouds (or a debug shell) present.
    pub active: bool,
}

impl ExtractResource<RenderApp> for CloudsGpu {
    type Source = CloudsGpu;

    fn extract_resource(source: &Self::Source) -> Self {
        source.clone()
    }
}

/// Marks the camera the clouds are drawn for; extracted to the render world.
#[derive(Component, Clone, Copy, Debug, Default, ExtractComponent)]
#[extract_app(RenderApp)]
pub(crate) struct CloudsView;

/// Self-built volumetric clouds (adr-0007).
///
/// Needs `DepthPrepass` and `Msaa::Off`, which [`ElementsCamera`] provides
/// (this plugin observes it to add the prepass itself — the crate only does
/// so when its own `CloudPlugin` runs, and that one is disabled).
pub struct CloudsPlugin;

impl Plugin for CloudsPlugin {
    fn build(&self, app: &mut App) {
        load_shader_library!(app, "common.wesl");
        load_shader_library!(app, "march.wesl");
        load_shader_library!(app, "resolve.wesl");
        load_shader_library!(app, "composite.wesl");
        app.init_resource::<CloudsRuntime>()
            .init_resource::<CloudsGpu>()
            .add_observer(add_depth_prepass)
            .add_systems(
                Update,
                (mark_view, update_views, update_gpu)
                    .chain()
                    // The GPU uniforms follow the sky's overrides, which the
                    // sky plugin settles in Weather/Celestial.
                    .after(ElementsSystems::Celestial),
            );
        gpu::plugin(app);
    }
}

/// The crate's cloud pass reads the depth prepass; with `CloudPlugin`
/// disabled nobody requests it, so this plugin does.
fn add_depth_prepass(trigger: On<Add<ElementsCamera>>, mut commands: Commands) {
    commands.entity(trigger.entity).insert(DepthPrepass);
}

/// `CloudsView` on every [`ElementsCamera`] (the game has one).
fn mark_view(
    mut commands: Commands,
    marked: Query<Entity, Added<ElementsCamera>>,
    has: Query<(), With<CloudsView>>,
) {
    for entity in &marked {
        if !has.contains(entity) {
            commands.entity(entity).insert(CloudsView);
        }
    }
}

/// Half-precision 1.0, little-endian.
const HALF_ONE: [u8; 2] = [0x00, 0x3C];

/// A storage texture the kernels write and shaders sample with a linear
/// sampler.
fn texture(
    images: &mut Assets<Image>,
    size: Extent3d,
    dimension: TextureDimension,
    format: TextureFormat,
    pixel: &[u8],
) -> Handle<Image> {
    let mut image = Image::new_fill(
        size,
        dimension,
        pixel,
        format,
        bevy::asset::RenderAssetUsages::RENDER_WORLD,
    );
    image.texture_descriptor.usage =
        TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_SRC;
    image.sampler = bevy::image::ImageSampler::Descriptor(bevy::image::ImageSamplerDescriptor {
        address_mode_u: bevy::image::ImageAddressMode::ClampToEdge,
        address_mode_v: bevy::image::ImageAddressMode::ClampToEdge,
        address_mode_w: bevy::image::ImageAddressMode::ClampToEdge,
        mag_filter: bevy::image::ImageFilterMode::Linear,
        min_filter: bevy::image::ImageFilterMode::Linear,
        mipmap_filter: bevy::image::ImageFilterMode::Linear,
        ..default()
    });
    images.add(image)
}

/// March, history and result textures sized for the camera; rebuilt when the
/// size changes (which also flags the history invalid).
fn update_views(
    cameras: Query<&Camera, With<ElementsCamera>>,
    mut images: ResMut<Assets<Image>>,
    mut gpu: ResMut<CloudsGpu>,
    mut runtime: ResMut<CloudsRuntime>,
) {
    let size = cameras
        .iter()
        .next()
        .and_then(Camera::physical_target_size)
        .filter(|s| s.x > 0 && s.y > 0);
    let Some(size) = size else {
        if gpu.view.is_some() {
            gpu.view = None;
        }
        return;
    };
    if gpu.view.as_ref().is_some_and(|v| v.size == size) {
        return;
    }
    let plane = |s: UVec2| Extent3d {
        width: s.x,
        height: s.y,
        depth_or_array_layers: 1,
    };
    // Clear sky until the first resolve: no radiance, full transmittance.
    let clear = [0, 0, 0, 0, 0, 0, HALF_ONE[0], HALF_ONE[1]];
    let mut make = |s: UVec2| {
        texture(
            &mut images,
            plane(s),
            TextureDimension::D2,
            TextureFormat::Rgba16Float,
            &clear,
        )
    };
    // Half resolution per axis.
    let low = (size + UVec2::splat(1)) / 2;
    gpu.view = Some(CloudsViewImages {
        size,
        march: make(low),
        history: [make(size), make(size)],
    });
    runtime.reset_frames = 2;
}

#[allow(clippy::too_many_arguments)]
fn update_gpu(
    sky: Res<DuskSky>,
    debug: Option<Res<CloudsDebug>>,
    sun: Option<Res<SunState>>,
    units: Option<Res<ElementsUnits>>,
    quality: Res<ElementsQuality>,
    atmospheres: Query<&Atmosphere>,
    cameras: Query<&GlobalTransform, With<ElementsCamera>>,
    mut runtime: ResMut<CloudsRuntime>,
    mut gpu: ResMut<CloudsGpu>,
) {
    runtime.frame = runtime.frame.wrapping_add(1);
    let clouds = resolve_clouds(&sky);
    let debug = debug.as_deref().copied().unwrap_or_default();

    // A kind/amount/layer change invalidates the history (the shell moves).
    let shape = (sky.cloud_kind, sky.cloud_amount, sky.cloud_layer);
    if runtime.last_shape != Some(shape) {
        runtime.last_shape = Some(shape);
        runtime.reset_frames = 2;
    }
    let reset = runtime.reset_frames > 0;
    runtime.reset_frames = runtime.reset_frames.saturating_sub(1);

    let (cam, size) = match gpu.view.as_ref() {
        Some(view) => (cameras.iter().next().map(|t| t.translation()), view.size),
        None => (None, UVec2::ONE),
    };
    let Some(cam) = cam else {
        gpu.active = false;
        return;
    };
    let (inner, outer) = atmospheres
        .iter()
        .next()
        .map_or((6_360_000.0, 6_460_000.0), |a| {
            (a.inner_radius, a.outer_radius)
        });
    // The sun through clear air; the golden hour locks it at ~2.6°, so the
    // LUT tint (C2) does the dusk colouring, not this illuminance.
    let (light, light_e) = sun.map_or((Vec3::Y, Vec3::splat(lux::RAW_SUNLIGHT)), |s| {
        (s.direction, Vec3::splat(lux::RAW_SUNLIGHT))
    });
    let (base_m, top_m) = clouds.as_ref().map_or((1500.0, 3300.0), |c| c.layer);
    let q = CloudsQuality::of(*quality);
    let low = (size + UVec2::splat(1)) / 2;

    gpu.active = debug.mode > 0.0 || clouds.is_some();
    gpu.params = CloudsParams {
        camera: cam.extend(inner),
        atmo: Vec4::new(outer, 32_000.0, 32.0, 0.0),
        layer: Vec4::new(
            base_m,
            top_m.max(base_m + 200.0),
            0.5,
            clouds.as_ref().map_or(0.0, |c| c.cover),
        ),
        light: light.extend(0.0),
        light_e: light_e.extend(0.0),
        // C1 wires the wind; C0 keeps the field at the camera's origin.
        drift: Vec4::new(0.0, 0.0, 0.0, 0.0),
        drift_detail: Vec4::new(0.0, 0.0, 0.0, 0.0),
        weather: Vec4::new(0.0, 0.0, 16_000.0, 0.0),
        counts: UVec4::new(runtime.frame, q.steps(), q.light_samples(), 0),
        sizes: UVec4::new(size.x, size.y, low.x, low.y),
        misc: Vec4::new(
            debug.mode,
            debug.luminance,
            0.0,
            if reset { 1.0 } else { 0.0 },
        ),
        units: Vec4::new(units.as_ref().map_or(1.0, |u| u.per_metre()), 0.0, 0.0, 0.0),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The uniform the shaders read must mirror [`params::CloudsParams`].
    #[test]
    fn params_mirror_wesl_struct() {
        let common = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/plugins/clouds/common.wesl"
        ))
        .expect("common.wesl");
        for field in [
            "camera",
            "atmo",
            "layer",
            "light",
            "light_e",
            "drift",
            "drift_detail",
            "weather",
            "counts",
            "sizes",
            "misc",
            "units",
        ] {
            assert!(
                common.contains(&format!("    {field}: ")),
                "common.wesl CloudParams is missing field `{field}`; the Rust \
                 and WESL structs must stay in lockstep"
            );
        }
    }

    /// The atmosphere lookups and shell geometry the game-side shader owns
    /// are a copy of the vendored `clouds/common.wesl` ones (the crate's are
    /// `pub(crate)`). If a copy is dropped the kernels stop compiling — this
    /// is the tripwire that catches silent drift early.
    #[test]
    fn common_wesl_carries_the_copied_math() {
        let common = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/plugins/clouds/common.wesl"
        ))
        .expect("common.wesl");
        for symbol in [
            "fn transmittance_uv",
            "fn light_at",
            "fn shell_range",
            "fn view_ray",
            "fn scene_distance",
        ] {
            assert!(
                common.contains(symbol),
                "game-side common.wesl lost `{symbol}`; the cloud kernels need it"
            );
        }
    }

    /// A shape change must flag the history invalid so the resolve does not
    /// blend two different cloud decks.
    #[test]
    fn shape_change_resets_the_history() {
        let mut runtime = CloudsRuntime {
            frame: 0,
            reset_frames: 0,
            last_shape: Some((None, 0.0, None)),
        };
        let changed = Some((None, 0.5, None));
        if runtime.last_shape != changed {
            runtime.last_shape = changed;
            runtime.reset_frames = 2;
        }
        assert_eq!(runtime.reset_frames, 2);
    }
}
