// clouds_march — the primary ray march at half resolution. C0 renders the
// debug shell (uniform luminance with a per-texel hash jitter: the pattern
// moves with the camera, which makes reprojection errors visible by eye);
// C1 replaces it with the real density model.
//
// Every texel is marched every frame (no Bayer slot): the fresh field is
// complete, which lets the resolve clamp against it directly.

import bevy_render::view::View;
import bevy_elements::noise::hash31;
import no_one_looks_up::plugins::clouds::common::{
    CloudParams, is_orthographic, scene_distance, shell_range, view_ray,
};

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> params: CloudParams;
@group(0) @binding(2) var depth_tex: texture_depth_2d;
@group(0) @binding(3) var out_tex: texture_storage_2d<rgba16float, write>;

@compute @workgroup_size(8, 8, 1)
fn clouds_march(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = params.sizes.xy;
    let low = vec2<u32>(params.sizes.zw);
    if any(id.xy >= low) {
        return;
    }
    // The texel's top-left pixel; its centre samples the shell of the 2×2 block.
    let px = vec2<f32>(id.xy * 2u) + vec2<f32>(0.5);
    let uv = px / vec2<f32>(size);
    let ray = view_ray(
        uv,
        view.world_from_clip,
        params.camera.xyz,
        is_orthographic(view.clip_from_view),
        params.camera.xyz,
        params.units.x,
    );
    let cam = ray.origin;
    let dir = ray.dir;
    let big_r = params.camera.w;
    let shell = shell_range(big_r + cam.y, dir.y, params.layer.x, params.layer.y, big_r);

    // Premultiplied (L, T): clear sky by default. The history stores
    // UNexposed luminance; the composite applies `view.exposure`.
    var out = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    if shell.y > shell.x {
        // A surface in front of the shell hides the cloud on this ray (the
        // composite would discard it anyway; skipping keeps the fresh field
        // consistent for the resolve).
        let depth = textureLoad(depth_tex, vec2<i32>(id.xy * 2u), 0);
        if !(depth > 0.0
            && scene_distance(uv, depth, view.world_from_clip, ray, params.units.x) < shell.x)
        {
            if params.misc.x > 0.5 {
                // Debug shell: a warm white with per-texel jitter so temporal
                // reprojection is visible by eye (a flat field hides it).
                let jitter = mix(0.75, 1.25, hash31(vec3<i32>(vec2<i32>(id.xy), 0i)));
                let lum = params.misc.y * jitter;
                out = vec4<f32>(vec3<f32>(1.0, 0.9, 0.75) * lum, 0.0);
            }
        }
    }
    textureStore(out_tex, vec2<i32>(id.xy), out);
}
