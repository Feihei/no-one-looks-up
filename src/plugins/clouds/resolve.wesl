// clouds_resolve — full resolution temporal resolve. The march covers every
// march texel every frame, so each pixel has a fresh bilinear upsample of it:
// the history is reprojected through the previous view (the shell entry point
// is a stable world-space anchor — camera translation at walking speed is
// sub-pixel at 1.5 km, rotation dominates), clamped to the 3×3 fresh box of
// its march texel so a changed or uncovered region cannot ghost, and blended
// toward the fresh sample.
//
// Unlike the vendored resolve there is no Bayer slot: no owner weighting, and
// the history stores UNexposed luminance so no exposure compensation is
// needed (the composite applies exposure; the sky's position perturbation
// never pollutes the history).

import bevy_render::view::View;
import no_one_looks_up::plugins::clouds::common::{
    CloudParams, is_orthographic, shell_range, view_ray,
};

/// Mirrors Bevy's `PreviousViewData`.
struct PreviousView {
    view_from_world: mat4x4<f32>,
    clip_from_world: mat4x4<f32>,
    unjittered_clip_from_world: mat4x4<f32>,
    clip_from_view: mat4x4<f32>,
    world_from_clip: mat4x4<f32>,
    view_from_clip: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> previous: PreviousView;
@group(0) @binding(2) var<uniform> params: CloudParams;
@group(0) @binding(3) var march_tex: texture_2d<f32>;
@group(0) @binding(4) var history_in: texture_2d<f32>;
@group(0) @binding(5) var smp: sampler;
@group(0) @binding(6) var history_out: texture_storage_2d<rgba16float, write>;

/// Weight of the fresh upsample in every pixel's history.
const FRESH_BLEND: f32 = 0.5;

@compute @workgroup_size(8, 8, 1)
fn clouds_resolve(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = params.sizes.xy;
    if any(id.xy >= size) {
        return;
    }
    let low = vec2<i32>(params.sizes.zw);
    // The march texel this pixel falls in, and its 3×3 fresh box.
    let block = min(vec2<i32>(id.xy / 2u), low - vec2<i32>(1));
    let uv = (vec2<f32>(id.xy) + vec2<f32>(0.5)) / vec2<f32>(size);
    // Fresh upsample of this frame's march (the history starts invalid).
    let fresh = textureSampleLevel(march_tex, smp, uv, 0.0);
    if params.misc.w > 0.5 {
        textureStore(history_out, vec2<i32>(id.xy), fresh);
        return;
    }

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
    if shell.y <= shell.x {
        textureStore(history_out, vec2<i32>(id.xy), vec4<f32>(0.0, 0.0, 0.0, 1.0));
        return;
    }

    // Where this pixel's cloud was on the previous screen: the point of the
    // shell it enters. The shell point is in metres; the previous view's
    // matrices take world units.
    let clip = previous.clip_from_world * vec4<f32>((cam + dir * shell.x) * params.units.x, 1.0);
    let ndc = clip.xy / clip.w;
    var resolved = fresh;
    if clip.w > 0.0 && all(abs(ndc) <= vec2<f32>(1.0)) {
        let history = textureSampleLevel(
            history_in,
            smp,
            vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5),
            0.0,
        );
        var lo = fresh;
        var hi = fresh;
        for (var j = -1; j <= 1; j += 1) {
            for (var i = -1; i <= 1; i += 1) {
                let n = textureLoad(
                    march_tex,
                    clamp(block + vec2<i32>(i, j), vec2<i32>(0), low - vec2<i32>(1)),
                    0,
                );
                lo = min(lo, n);
                hi = max(hi, n);
            }
        }
        resolved = mix(clamp(history, lo, hi), fresh, FRESH_BLEND);
    }
    textureStore(history_out, vec2<i32>(id.xy), resolved);
}
