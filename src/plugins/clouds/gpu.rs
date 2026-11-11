//! Render-world side of the game's clouds: pipelines, bind groups, dispatches.
//!
//! Adapted from the vendored bevy_elements `clouds/gpu.rs` (its helpers are
//! `pub(crate)`), minus everything the game's model does not have: no shadow
//! map, no sky probe, no histogram. C0 renders the debug shell only; the
//! noise slabs and weather map arrive with C1.

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

use bevy::asset::load_embedded_asset;
use bevy::core_pipeline::core_3d::{main_opaque_pass_3d, main_transparent_pass_3d};
use bevy::core_pipeline::prepass::{
    PreviousViewData, PreviousViewUniformOffset, PreviousViewUniforms, ViewPrepassTextures,
};
use bevy::core_pipeline::schedule::camera_driver;
use bevy::core_pipeline::{Core3d, Core3dSystems, FullscreenShader};
use bevy::prelude::*;
use bevy::render::diagnostic::RecordDiagnostics;
use bevy::render::extract_resource::ExtractResourcePlugin;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::binding_types::{
    sampler, texture_2d, texture_depth_2d, texture_storage_2d, uniform_buffer,
};
use bevy::render::render_resource::{
    AddressMode, BindGroup, BindGroupEntries, BindGroupLayoutDescriptor, BindGroupLayoutEntries,
    BlendComponent, BlendFactor, BlendOperation, BlendState, CachedComputePipelineId,
    CachedPipelineState, CachedRenderPipelineId, ColorTargetState, ColorWrites,
    ComputePassDescriptor, ComputePipeline, ComputePipelineDescriptor, FilterMode, FragmentState,
    LoadOp, MipmapFilterMode, Operations, PipelineCache, RenderPassColorAttachment,
    RenderPassDescriptor, RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor,
    ShaderStages, StorageTextureAccess, StoreOp, TextureFormat, TextureSampleType, UniformBuffer,
};
use bevy::render::renderer::{RenderContext, RenderDevice, RenderQueue, ViewQuery};
use bevy::render::texture::GpuImage;
use bevy::render::view::{ViewTarget, ViewUniform, ViewUniformOffset, ViewUniforms};
use bevy::render::{Render, RenderApp, RenderStartup, RenderSystems};
use bevy::shader::{Shader, ShaderCacheError, ShaderDefVal};

use super::{CloudsGpu, CloudsParams, CloudsView, CloudsViewImages};

/// Workgroup edge of the 2D kernels (`@workgroup_size(8, 8, 1)`).
const WG2: u32 = 8;

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
    march: Kernel,
    resolve: Kernel,
    composite: CachedRenderPipelineId,
    composite_layout: BindGroupLayoutDescriptor,
    clamp: Sampler,
}

#[derive(Resource, Default)]
struct CloudsUniform(UniformBuffer<CloudsParams>);

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
        bevy::render::extract_component::ExtractComponentPlugin::<CloudsView>::default(),
    ));
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app
        .init_resource::<CloudsUniform>()
        .add_systems(RenderStartup, init_pipelines)
        .add_systems(
            Render,
            prepare_global.in_set(RenderSystems::PrepareBindGroups),
        )
        .add_systems(RenderGraph, prepare_views.before(camera_driver))
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

#[allow(clippy::too_many_arguments)]
fn init_pipelines(
    mut commands: Commands,
    assets: Res<AssetServer>,
    cache: Res<PipelineCache>,
    device: Res<RenderDevice>,
    fullscreen: Res<FullscreenShader>,
) {
    let write = StorageTextureAccess::WriteOnly;
    let compute = ShaderStages::COMPUTE;
    let march_layout = BindGroupLayoutDescriptor::new(
        "no_one_clouds_march",
        &BindGroupLayoutEntries::sequential(
            compute,
            (
                uniform_buffer::<ViewUniform>(true),
                uniform_buffer::<CloudsParams>(false),
                texture_depth_2d(),
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

    let clamp = device.create_sampler(&SamplerDescriptor {
        address_mode_u: AddressMode::ClampToEdge,
        address_mode_v: AddressMode::ClampToEdge,
        address_mode_w: AddressMode::ClampToEdge,
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        mipmap_filter: MipmapFilterMode::Linear,
        ..default()
    });
    commands.insert_resource(CloudsPipelines {
        march: kernel(march, "clouds_march", &march_layout),
        resolve: kernel(resolve, "clouds_resolve", &resolve_layout),
        composite,
        composite_layout,
        clamp,
    });
}

/// Write the uniform buffer the per-view bind groups reference.
fn prepare_global(
    mut cloud: Option<ResMut<CloudsGpu>>,
    mut uniform: ResMut<CloudsUniform>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
) {
    let Some(cloud) = cloud.as_mut() else {
        return;
    };
    uniform.0.set(cloud.params);
    uniform.0.write_buffer(&device, &queue);
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
    views: Query<(Entity, &ViewPrepassTextures, &Msaa), With<CloudsView>>,
) {
    let (Some(cloud), Some(pipelines)) = (cloud, pipelines) else {
        return;
    };
    let Some(view_images) = cloud.view.as_ref() else {
        return;
    };
    let (Some(view_binding), Some(previous_binding)) = (
        view_uniforms.uniforms.binding(),
        previous_uniforms.uniforms.binding(),
    ) else {
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
    for (entity, prepass, msaa) in &views {
        // The layouts bind a single-sampled depth and the composite writes the
        // main texture without a resolve: a camera that kept its MSAA
        // (`ElementsCamera::msaa_off` off) is skipped.
        let Some(depth) = prepass.depth_only_view().filter(|_| *msaa == Msaa::Off) else {
            continue;
        };
        let march_group = device.create_bind_group(
            "no_one_clouds_march",
            &cache.get_bind_group_layout(&pipelines.march.layout),
            &BindGroupEntries::sequential((
                view_binding.clone(),
                &uniform.0,
                depth,
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

fn clouds_march(
    view: ViewQuery<(&ViewUniformOffset, &CloudsViewGroups)>,
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
