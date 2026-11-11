// cloud_composite — composite the resolved cloud over the scene:
// `scene · T + L` with the blend `src.rgb + dst.rgb · src.a`. A pixel whose
// scene surface is nearer than the cloud entry of its ray shows no cloud.
// The history is UNexposed luminance; exposure is applied here, so the sky's
// exposure perturbation cannot pollute the history.
//
// C2 adds the aerial perspective and the sky gain here (sky_through_clouds).

import bevy_core_pipeline::fullscreen_vertex_shader::FullscreenVertexOutput;
import bevy_render::view::View;
import no_one_looks_up::plugins::clouds::common::{
    CloudParams, is_orthographic, scene_distance, shell_range, view_ray,
};

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> params: CloudParams;
@group(0) @binding(2) var history: texture_2d<f32>;
@group(0) @binding(3) var depth_tex: texture_depth_2d;

@fragment
fn cloud_composite(in: FullscreenVertexOutput) -> @location(0) vec4<f32> {
    let pixel = vec2<i32>(in.position.xy);
    let depth = textureLoad(depth_tex, pixel, 0);
    let ray = view_ray(
        in.uv,
        view.world_from_clip,
        params.camera.xyz,
        is_orthographic(view.clip_from_view),
        params.camera.xyz,
        params.units.x,
    );
    let cam = ray.origin;
    let dir = ray.dir;
    if depth > 0.0 {
        let big_r = params.camera.w;
        let shell = shell_range(big_r + cam.y, dir.y, params.layer.x, params.layer.y, big_r);
        if scene_distance(in.uv, depth, view.world_from_clip, ray, params.units.x) < shell.x {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }
    let cloud = textureLoad(history, pixel, 0);
    return vec4<f32>(cloud.rgb * view.exposure, cloud.a);
}
