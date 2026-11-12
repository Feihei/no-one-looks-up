//! Render-world side of the game's clouds: pipelines, bind groups, dispatches.
//!
//! Adapted from the vendored bevy_elements `clouds/gpu.rs` (its helpers are
//! `pub(crate)`), minus everything the game's model does not have: no shadow
//! map, no sky probe, no precipitation.
//!
//! View-independent kernels (noise, weather map) run in the `RenderGraph`
//! schedule before any camera draws; the per-view march and resolve run in
//! `Core3d` before the opaque pass (the depth prepass and Bevy's atmosphere
//! LUTs are ready), and the composite draws after the transparent pass.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use bevy::asset::load_embedded_asset;
use bevy::core_pipeline::core_3d::{main_opaque_pass_3d, main_transparent_pass_3d};
use bevy::core_pipeline::prepass::{
    PreviousViewData, PreviousViewUniformOffset, PreviousViewUniforms, ViewPrepassTextures,
};
use bevy::core_pipeline::schedule::camera_driver;
use bevy::core_pipeline::{Core3d, Core3dSystems, FullscreenShader};
use bevy::pbr::resources::AtmosphereTextures;
use bevy::prelude::*;
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::extract_component::ExtractComponentPlugin;
use bevy::render::extract_resource::ExtractResourcePlugin;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{
    sampler, storage_buffer_sized, texture_2d, texture_3d, texture_cube, texture_depth_2d,
    texture_storage_2d, texture_storage_3d, uniform_buffer,
};
use bevy::render::render_resource::{
    AddressMode, BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
    BlendComponent, BlendFactor, BlendOperation, BlendState, Buffer, BufferBinding,
    BufferDescriptor, CachedComputePipelineId, CachedPipelineState, CachedRenderPipelineId,
    ColorTargetState, ColorWrites, ComputePassDescriptor, ComputePipeline,
    ComputePipelineDescriptor, Extent3d, FilterMode, FragmentState, LoadOp, MipmapFilterMode,
    Operations, PipelineCache, RenderPassColorAttachment, RenderPassDescriptor,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderStages,
    StorageTextureAccess, StoreOp, TextureDescriptor, TextureDimension, TextureFormat,
    TextureSampleType, TextureUsages, TextureView, TextureViewDescriptor, UniformBuffer,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::{ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use bevy::shader::{Shader, ShaderCacheError, ShaderDefVal};

use super::config::{NOISE_DETAIL_TEXELS, NOISE_PASSES, NOISE_SLAB, WEATHER_TEXELS};
use super::{CloudsGpu, CloudsParams, CloudsView, CloudsViewImages};

/// Workgroup edge of the 2D kernels (`@workgroup_size(8, 8, 1)`).
const WG2: u32 = 8;
/// Workgroup edge of the 3D kernels (`@workgroup_size(4, 4, 4)`).
const WG3: u32 = 4;

// ---------------------------------------------------------------------------
// Kernel helper (the vendor's lives in a `pub(crate)` module).
// ---------------------------------------------------------------------------

/// One compute entry point, queued in the pipeline cache.
struct Kernel {
    id: CachedComputePipelineId,
    /// The layout of bind group 0.
    layout: BindGroupLayoutDescriptor,
    entry: &'static str,
    reported: AtomicBool,
}

/// Queue `entry` of `shader` with bind group 0 laid out as `layout`.
fn queue_kernel(
    cache: &PipelineCache,
    shader: Handle<Shader>,
    entry: &'static str,
    layout: BindGroupLayoutDescriptor,
    defs: Vec<ShaderDefVal>,
) -> Kernel {
    let id = cache.queue_compute_pipeline(ComputePipelineDescriptor {
        label: Some(Cow::Borrowed(entry)),
        layout: vec![layout.clone()],
        shader,
        shader_defs: defs,
        entry_point: Some(Cow::Borrowed(entry)),
        zero_initialize_workgroup_memory: true,
        ..default()
    });
    Kernel {
        id,
        layout,
        entry,
        reported: AtomicBool::new(false),
    }
}

/// The compiled pipeline, or `None` while it compiles or after it failed.
fn ready<'a>(cache: &'a PipelineCache, kernel: &Kernel) -> Option<&'a ComputePipeline> {
    match cache.get_compute_pipeline_state(kernel.id) {
        CachedPipelineState::Ok(_) => cache.get_compute_pipeline(kernel.id),
        CachedPipelineState::Err(
            ShaderCacheError::ShaderNotLoaded(_) | ShaderCacheError::ShaderImportNotYetAvailable,
        ) => None,
        CachedPipelineState::Err(error) => {
            if !kernel.reported.swap(true, Ordering::Relaxed) {
                error!(
                    "clouds kernel `{}` failed to compile and the clouds will stay idle: {error}",
                    kernel.entry
                );
            }
            None
        }
        CachedPipelineState::Queued | CachedPipelineState::Creating(_) => None,
    }
}

/// Workgroups needed to cover `cells` with groups of `edge`.
fn groups(cells: u32, edge: u32) -> u32 {
    cells.div_ceil(edge)
}

// ---------------------------------------------------------------------------
// Pipelines and resources.
// ---------------------------------------------------------------------------

#[derive(Resource)]
struct CloudsPipelines {
    noise_base: Kernel,
    noise_detail: Kernel,
    weather_clear: Kernel,
    weather_field: Kernel,
    weather_threshold: Kernel,
    weather: Kernel,
    march: Kernel,
    resolve: Kernel,
    composite: CachedRenderPipelineId,
    composite_layout: BindGroupLayoutDescriptor,
    /// Filtering + repeat, for the noise volumes and the weather map.
    repeat: Sampler,
    /// Filtering + clamp, for the atmosphere LUTs and the history.
    clamp: Sampler,
    /// The weather map's own target, and the histogram the threshold pass reads.
    weather_target: TextureView,
    histogram: Buffer,
    threshold: Buffer,
    /// A zero-filled cube bound when the camera has no environment map yet
    /// (the march then falls back to a sun-derived ambient luminance).
    empty_cube: TextureView,
}

#[derive(Resource, Default)]
struct CloudsUniform(UniformBuffer<CloudsParams>);

/// The kernels' shared bind groups. Both are static once the noise volumes are
/// uploaded: they bind buffer and texture identities, not their contents.
#[derive(Resource, Default)]
struct CloudsGlobal {
    noise: Option<BindGroup>,
    weather: Option<BindGroup>,
}

/// How many noise slabs have been generated.
#[derive(Resource, Default)]
struct CloudsProgress {
    noise_passes: u32,
}

/// Bind groups of the per-view passes, rebuilt every frame (the view uniform
/// buffers can be re-created between frames).
#[derive(Component)]
struct CloudsViewGroups {
    march: BindGroup,
    resolve: BindGroup,
    composite: BindGroup,
}

pub(super) fn plugin(app: &mut App) {
    app.add_plugins((
        ExtractResourcePlugin::<CloudsGpu>::default(),
        ExtractComponentPlugin::<CloudsView>::default(),
    ));
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app
        .init_resource::<CloudsUniform>()
        .init_resource::<CloudsGlobal>()
        .init_resource::<CloudsProgress>()
        .add_systems(RenderStartup, init_pipelines)
        .add_systems(
            Render,
            (prepare_global, prepare_views)
                .chain()
                .in_set(RenderSystems::PrepareBindGroups),
        )
        .add_systems(RenderGraph, dispatch_global.before(camera_driver))
        .add_systems(
            Core3d,
            (
                (clouds_march, clouds_resolve)
                    .chain()
                    .in_set(Core3dSystems::MainPass)
                    .before(main_opaque_pass_3d),
                clouds_composite
                    .in_set(Core3dSystems::MainPass)
                    .after(main_transparent_pass_3d),
            ),
        );
}

fn init_pipelines(
    mut commands: Commands,
    assets: Res<AssetServer>,
    cache: Res<PipelineCache>,
    device: Res<RenderDevice>,
    fullscreen: Res<FullscreenShader>,
) {
    let write = StorageTextureAccess::WriteOnly;
    let compute = ShaderStages::COMPUTE;

    let noise_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_noise",
        &BindGroupLayoutEntries::sequential(
            compute,
            (
                uniform_buffer::<CloudsParams>(false),
                texture_storage_3d(TextureFormat::Rgba8Unorm, write),
                texture_storage_3d(TextureFormat::Rgba8Unorm, write),
            ),
        ),
    );
    let weather_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_weather",
        &BindGroupLayoutEntries::sequential(
            compute,
            (
                uniform_buffer::<CloudsParams>(false),
                storage_buffer_sized(false, None),
                storage_buffer_sized(false, None),
                texture_storage_2d(TextureFormat::Rgba8Unorm, write),
            ),
        ),
    );
    let march_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_march",
        &BindGroupLayoutEntries::sequential(
            compute,
            (
                uniform_buffer::<ViewUniform>(true),
                uniform_buffer::<CloudsParams>(false),
                texture_3d(TextureSampleType::Float { filterable: true }),
                texture_3d(TextureSampleType::Float { filterable: true }),
                texture_2d(TextureSampleType::Float { filterable: true }),
                texture_2d(TextureSampleType::Float { filterable: true }),
                texture_3d(TextureSampleType::Float { filterable: true }),
                texture_cube(TextureSampleType::Float { filterable: true }),
                texture_depth_2d(),
                sampler(SamplerBindingType::Filtering),
                sampler(SamplerBindingType::Filtering),
                texture_storage_2d(TextureFormat::Rgba16Float, write),
            ),
        ),
    );
    let resolve_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_resolve",
        &BindGroupLayoutEntries::sequential(
            compute,
            (
                uniform_buffer::<ViewUniform>(true),
                uniform_buffer::<PreviousViewData>(true),
                uniform_buffer::<CloudsParams>(false),
                texture_2d(TextureSampleType::Float { filterable: true }),
                texture_2d(TextureSampleType::Float { filterable: true }),
                sampler(SamplerBindingType::Filtering),
                texture_storage_2d(TextureFormat::Rgba16Float, write),
            ),
        ),
    );
    let composite_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_composite",
        &BindGroupLayoutEntries::sequential(
            ShaderStages::FRAGMENT,
            (
                uniform_buffer::<ViewUniform>(true),
                uniform_buffer::<CloudsParams>(false),
                texture_2d(TextureSampleType::Float { filterable: true }),
                texture_depth_2d(),
            ),
        ),
    );

    let kernel = |file: Handle<Shader>, entry: &'static str, layout: &BindGroupLayoutDescriptor| {
        queue_kernel(&cache, file, entry, layout.clone(), Vec::new())
    };
    let generate = load_embedded_asset!(assets.as_ref(), "generate.wesl");
    let weather = load_embedded_asset!(assets.as_ref(), "weather.wesl");
    let march = load_embedded_asset!(assets.as_ref(), "march.wesl");
    let resolve = load_embedded_asset!(assets.as_ref(), "resolve.wesl");
    let composite_shader = load_embedded_asset!(assets.as_ref(), "composite.wesl");

    // `scene · T + L`: the shaders output (L, T); colour `src + dst · src.a`,
    // alpha untouched.
    let blend = BlendState {
        color: BlendComponent {
            src_factor: BlendFactor::One,
            dst_factor: BlendFactor::SrcAlpha,
            operation: BlendOperation::Add,
        },
        alpha: BlendComponent {
            src_factor: BlendFactor::Zero,
            dst_factor: BlendFactor::One,
            operation: BlendOperation::Add,
        },
    };
    let composite = cache.queue_render_pipeline(RenderPipelineDescriptor {
        label: Some("no_one_clouds_composite".into()),
        layout: vec![composite_layout.clone()],
        vertex: fullscreen.to_vertex_state(),
        fragment: Some(FragmentState {
            shader: composite_shader,
            entry_point: Some("cloud_composite".into()),
            targets: vec![Some(ColorTargetState {
                // `ElementsCamera` requires `Hdr`.
                format: TextureFormat::Rgba16Float,
                blend: Some(blend),
                write_mask: ColorWrites::ALL,
            })],
            ..default()
        }),
        ..default()
    });

    let sampler = |address: AddressMode| {
        device.create_sampler(&SamplerDescriptor {
            address_mode_u: address,
            address_mode_v: address,
            address_mode_w: address,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Linear,
            ..default()
        })
    };

    let weather_texture = device.create_texture(&TextureDescriptor {
        label: Some("no_one_clouds_weather"),
        size: Extent3d {
            width: WEATHER_TEXELS,
            height: WEATHER_TEXELS,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let weather_target = weather_texture.create_view(&TextureViewDescriptor::default());
    let empty_cube_texture = device.create_texture(&TextureDescriptor {
        label: Some("no_one_clouds_empty_cube"),
        size: Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 6,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba16Float,
        usage: TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let empty_cube = empty_cube_texture.create_view(&TextureViewDescriptor {
        dimension: Some(bevy::render::render_resource::TextureViewDimension::Cube),
        ..default()
    });
    // 256 bins of u32, and the single threshold they reduce to (the space of 4
    // is the vendored layout, kept so the WESL array sizes match).
    let histogram = device.create_buffer(&BufferDescriptor {
        label: Some("no_one_clouds_histogram"),
        size: 256 * 4,
        usage: bevy::render::render_resource::BufferUsages::STORAGE
            | bevy::render::render_resource::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let threshold = device.create_buffer(&BufferDescriptor {
        label: Some("no_one_clouds_threshold"),
        size: 4 * 4,
        usage: bevy::render::render_resource::BufferUsages::STORAGE
            | bevy::render::render_resource::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    commands.insert_resource(CloudsPipelines {
        noise_base: kernel(generate.clone(), "cloud_noise_base", &noise_layout),
        noise_detail: kernel(generate.clone(), "cloud_noise_detail", &noise_layout),
        weather_clear: kernel(weather.clone(), "cloud_histogram_clear", &weather_layout),
        weather_field: kernel(weather.clone(), "cloud_field", &weather_layout),
        weather_threshold: kernel(weather.clone(), "cloud_threshold", &weather_layout),
        weather: kernel(weather.clone(), "cloud_weather", &weather_layout),
        march: kernel(march, "clouds_march", &march_layout),
        resolve: kernel(resolve, "clouds_resolve", &resolve_layout),
        composite,
        composite_layout,
        repeat: sampler(AddressMode::Repeat),
        clamp: sampler(AddressMode::ClampToEdge),
        weather_target,
        histogram,
        threshold,
        empty_cube,
    });
}

/// Write the uniform buffer the bind groups reference, then build the shared
/// bind groups once their textures are uploaded.
#[allow(clippy::too_many_arguments)]
fn prepare_global(
    mut cloud: Option<ResMut<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    mut uniform: ResMut<CloudsUniform>,
    mut global: ResMut<CloudsGlobal>,
    progress: Res<CloudsProgress>,
    images: Res<RenderAssets<GpuImage>>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    cache: Res<PipelineCache>,
) {
    let (Some(cloud), Some(pipelines)) = (cloud.as_mut(), pipelines) else {
        return;
    };
    // The noise kernels read the slab's first slice from the uniform; the
    // weather pass ignores it.
    let mut params = cloud.params;
    params.counts.w = progress.noise_passes.min(NOISE_PASSES) * NOISE_SLAB;
    uniform.0.set(params);
    uniform.0.write_buffer(&device, &queue);

    if global.noise.is_none()
        && let Some(textures) = cloud.textures.as_ref()
        && let (Some(base), Some(detail)) =
            (images.get(&textures.base), images.get(&textures.detail))
    {
        global.noise = Some(device.create_bind_group(
            "no_one_clouds_noise",
            &cache.get_bind_group_layout(&pipelines.noise_base.layout),
            &BindGroupEntries::sequential((&uniform.0, &base.texture_view, &detail.texture_view)),
        ));
    }
    if global.weather.is_none() {
        global.weather = Some(device.create_bind_group(
            "no_one_clouds_weather",
            &cache.get_bind_group_layout(&pipelines.weather_field.layout),
            &BindGroupEntries::sequential((
                &uniform.0,
                BufferBinding {
                    buffer: &pipelines.histogram,
                    offset: 0,
                    size: None,
                },
                BufferBinding {
                    buffer: &pipelines.threshold,
                    offset: 0,
                    size: None,
                },
                &pipelines.weather_target,
            )),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_views(
    mut commands: Commands,
    cloud: Option<Res<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    uniform: Res<CloudsUniform>,
    view_uniforms: Res<ViewUniforms>,
    previous_uniforms: Res<PreviousViewUniforms>,
    images: Res<RenderAssets<GpuImage>>,
    cache: Res<PipelineCache>,
    device: Res<RenderDevice>,
    views: Query<
        (
            Entity,
            &ViewPrepassTextures,
            &Msaa,
            Option<&AtmosphereTextures>,
        ),
        With<CloudsView>,
    >,
) {
    let (Some(cloud), Some(pipelines)) = (cloud, pipelines) else {
        return;
    };
    let (Some(view_images), Some(textures)) = (cloud.view.as_ref(), cloud.textures.as_ref()) else {
        return;
    };
    let (Some(view_binding), Some(previous_binding)) = (
        view_uniforms.uniforms.binding(),
        previous_uniforms.uniforms.binding(),
    ) else {
        return;
    };
    let (Some(base), Some(detail)) = (images.get(&textures.base), images.get(&textures.detail))
    else {
        return;
    };
    let CloudsViewImages { march, history, .. } = view_images;
    let (Some(march), Some(history_a), Some(history_b)) = (
        images.get(march),
        images.get(&history[0]),
        images.get(&history[1]),
    ) else {
        return;
    };
    // The resolve writes history[frame % 2] and reads the other; the composite
    // shows the one just written.
    let (write, read) = if cloud.params.counts.x.is_multiple_of(2) {
        (history_a, history_b)
    } else {
        (history_b, history_a)
    };
    // The camera's atmosphere environment map, or the empty cube until Bevy has
    // generated it (the march then uses its sun-derived ambient floor).
    let env_view = cloud
        .env_map
        .as_ref()
        .and_then(|handle| images.get(handle))
        .map_or(&pipelines.empty_cube, |image| &image.texture_view);
    for (entity, prepass, msaa, atmosphere) in &views {
        // The layouts bind a single-sampled depth and the composite writes the
        // main texture without a resolve: a camera that kept its MSAA
        // (`ElementsCamera` sets it off) is skipped.
        let Some(depth) = prepass.depth_only_view().filter(|_| *msaa == Msaa::Off) else {
            continue;
        };
        let Some(atmosphere) = atmosphere else {
            continue;
        };
        let march_group = device.create_bind_group(
            "no_one_clouds_march",
            &cache.get_bind_group_layout(&pipelines.march.layout),
            &BindGroupEntries::sequential((
                view_binding.clone(),
                &uniform.0,
                &base.texture_view,
                &detail.texture_view,
                // The weather map is a render-world target, written by the
                // weather kernels every frame.
                &pipelines.weather_target,
                &atmosphere.transmittance_lut.default_view,
                &atmosphere.aerial_view_lut.default_view,
                env_view,
                depth,
                &pipelines.repeat,
                &pipelines.clamp,
                &march.texture_view,
            )),
        );
        let resolve_group = device.create_bind_group(
            "no_one_clouds_resolve",
            &cache.get_bind_group_layout(&pipelines.resolve.layout),
            &BindGroupEntries::sequential((
                view_binding.clone(),
                previous_binding.clone(),
                &uniform.0,
                &march.texture_view,
                &read.texture_view,
                &pipelines.clamp,
                &write.texture_view,
            )),
        );
        let composite_group = device.create_bind_group(
            "no_one_clouds_composite",
            &cache.get_bind_group_layout(&pipelines.composite_layout),
            &BindGroupEntries::sequential((
                view_binding.clone(),
                &uniform.0,
                &write.texture_view,
                depth,
            )),
        );
        commands.entity(entity).insert(CloudsViewGroups {
            march: march_group,
            resolve: resolve_group,
            composite: composite_group,
        });
    }
}

/// Generate the noise volumes, one z slab per frame, then the weather map every
/// frame. Runs before any camera draws.
fn dispatch_global(
    mut ctx: RenderContext,
    cache: Res<PipelineCache>,
    cloud: Option<Res<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    global: Option<Res<CloudsGlobal>>,
    mut progress: ResMut<CloudsProgress>,
) {
    let (Some(cloud), Some(pipelines), Some(global)) = (cloud, pipelines, global) else {
        return;
    };
    if !cloud.active {
        return;
    }
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();

    if progress.noise_passes < NOISE_PASSES {
        let Some(noise) = global.noise.as_ref() else {
            return;
        };
        let (Some(base), Some(detail)) = (
            ready(&cache, &pipelines.noise_base),
            ready(&cache, &pipelines.noise_detail),
        ) else {
            return;
        };
        let mut pass = ctx
            .command_encoder()
            .begin_compute_pass(&ComputePassDescriptor {
                label: Some("no_one_clouds_noise"),
                ..default()
            });
        let span = diagnostics.pass_span(&mut pass, "no_one_clouds_noise");
        pass.set_bind_group(0, noise, &[]);
        pass.set_pipeline(base);
        pass.dispatch_workgroups(
            groups(super::config::NOISE_BASE_TEXELS, WG3),
            groups(super::config::NOISE_BASE_TEXELS, WG3),
            groups(NOISE_SLAB, WG3),
        );
        if progress.noise_passes == 0 {
            pass.set_pipeline(detail);
            let n = groups(NOISE_DETAIL_TEXELS, WG3);
            pass.dispatch_workgroups(n, n, n);
        }
        span.end(&mut pass);
        progress.noise_passes += 1;
        return;
    }

    let Some(weather) = global.weather.as_ref() else {
        return;
    };
    let (Some(clear), Some(field), Some(threshold), Some(weather_pass)) = (
        ready(&cache, &pipelines.weather_clear),
        ready(&cache, &pipelines.weather_field),
        ready(&cache, &pipelines.weather_threshold),
        ready(&cache, &pipelines.weather),
    ) else {
        return;
    };
    let mut pass = ctx
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor {
            label: Some("no_one_clouds_weather"),
            ..default()
        });
    let span = diagnostics.pass_span(&mut pass, "no_one_clouds_weather");
    pass.set_bind_group(0, weather, &[]);
    let n = groups(WEATHER_TEXELS, WG2);
    pass.set_pipeline(clear);
    pass.dispatch_workgroups(1, 1, 1);
    pass.set_pipeline(field);
    pass.dispatch_workgroups(n, n, 1);
    pass.set_pipeline(threshold);
    pass.dispatch_workgroups(1, 1, 1);
    pass.set_pipeline(weather_pass);
    pass.dispatch_workgroups(n, n, 1);
    span.end(&mut pass);
}

fn noise_ready(progress: &CloudsProgress) -> bool {
    progress.noise_passes >= NOISE_PASSES
}

fn clouds_march(
    view: ViewQuery<(&ViewUniformOffset, &CloudsViewGroups)>,
    cloud: Option<Res<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    progress: Res<CloudsProgress>,
    cache: Res<PipelineCache>,
    mut ctx: RenderContext,
) {
    let (Some(cloud), Some(pipelines)) = (cloud, pipelines) else {
        return;
    };
    // The debug shell needs no noise, so the C0 check still runs at startup.
    if !cloud.active || (!noise_ready(&progress) && cloud.params.misc.x < 0.5) {
        return;
    }
    let (offset, bind) = view.into_inner();
    let Some(pipeline) = ready(&cache, &pipelines.march) else {
        return;
    };
    let low = cloud.params.sizes.zw();
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let mut pass = ctx
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor {
            label: Some("no_one_clouds_march"),
            ..default()
        });
    let span = diagnostics.pass_span(&mut pass, "no_one_clouds_march");
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind.march, &[offset.offset]);
    pass.dispatch_workgroups(groups(low.x, WG2), groups(low.y, WG2), 1);
    span.end(&mut pass);
}

fn clouds_resolve(
    view: ViewQuery<(
        &ViewUniformOffset,
        &PreviousViewUniformOffset,
        &CloudsViewGroups,
    )>,
    cloud: Option<Res<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    cache: Res<PipelineCache>,
    mut ctx: RenderContext,
) {
    let (Some(cloud), Some(pipelines)) = (cloud, pipelines) else {
        return;
    };
    if !cloud.active {
        return;
    }
    let (offset, previous, bind) = view.into_inner();
    let Some(pipeline) = ready(&cache, &pipelines.resolve) else {
        return;
    };
    let size = cloud.params.sizes.xy();
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let mut pass = ctx
        .command_encoder()
        .begin_compute_pass(&ComputePassDescriptor {
            label: Some("no_one_clouds_resolve"),
            ..default()
        });
    let span = diagnostics.pass_span(&mut pass, "no_one_clouds_resolve");
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind.resolve, &[offset.offset, previous.offset]);
    pass.dispatch_workgroups(groups(size.x, WG2), groups(size.y, WG2), 1);
    span.end(&mut pass);
}

fn clouds_composite(
    view: ViewQuery<(&ViewTarget, &ViewUniformOffset, &CloudsViewGroups)>,
    cloud: Option<Res<CloudsGpu>>,
    pipelines: Option<Res<CloudsPipelines>>,
    cache: Res<PipelineCache>,
    mut ctx: RenderContext,
) {
    let (Some(cloud), Some(pipelines)) = (cloud, pipelines) else {
        return;
    };
    if !cloud.active {
        return;
    }
    let (target, offset, bind) = view.into_inner();
    let Some(pipeline) = cache.get_render_pipeline(pipelines.composite) else {
        return;
    };
    let diagnostics = ctx.diagnostic_recorder();
    let diagnostics = diagnostics.as_deref();
    let mut pass = ctx
        .command_encoder()
        .begin_render_pass(&RenderPassDescriptor {
            label: Some("no_one_clouds_composite"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target.main_texture_view(),
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    let span = diagnostics.pass_span(&mut pass, "no_one_clouds_composite");
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, &bind.composite, &[offset.offset]);
    pass.draw(0..3, 0..1);
    span.end(&mut pass);
}
