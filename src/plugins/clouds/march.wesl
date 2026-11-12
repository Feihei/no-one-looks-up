// clouds_march — the primary ray march at half resolution.
//
// Every texel is marched every frame (no Bayer slot): the fresh field is
// complete, which lets the resolve clamp against it directly. A coarse pass
// skips empty air in 2·dt steps and drops to fine steps when the base shape is
// non-zero, so most of the cost is spent inside cloud.
//
// Output: premultiplied (L, T) in UNexposed luminance. The composite applies
// the view exposure, so the sky's position perturbation never pollutes the
// history.

import bevy_render::view::View;
import bevy_elements::noise::hash31;
import no_one_looks_up::plugins::clouds::common::{
    CloudParams, MAX_MARCH_M, altitude, cloud_sigma, coarse_shape, detail_weight, is_orthographic,
    light_at, phase, scene_distance, shell_range, view_ray,
};

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> params: CloudParams;
@group(0) @binding(2) var base_tex: texture_3d<f32>;
@group(0) @binding(3) var detail_tex: texture_3d<f32>;
@group(0) @binding(4) var weather_tex: texture_2d<f32>;
@group(0) @binding(5) var transmittance_lut: texture_2d<f32>;
@group(0) @binding(6) var depth_tex: texture_depth_2d;
@group(0) @binding(7) var smp_repeat: sampler;
@group(0) @binding(8) var smp_clamp: sampler;
@group(0) @binding(9) var march_out: texture_storage_2d<rgba16float, write>;

/// Consecutive empty fine samples after which the march returns to coarse
/// steps (Nubis).
const EMPTY_LIMIT: u32 = 10u;
/// Transmittance below which the march stops (Schneider's alpha → 1).
const MIN_TRANSMITTANCE: f32 = 0.01;
/// Tangent of the light-sample cone half angle.
const CONE_TAN: f32 = 0.1;
/// Bottom weight of the ambient gradient (the ground's bounce into the base).
const AMBIENT_BOTTOM: f32 = 0.2;

/// A marched ray: premultiplied scattered luminance and transmittance.
struct Integrated {
    radiance: vec3<f32>,
    transmittance: f32,
}

/// The medium at a point of altitude `h` above world XZ `xz`.
fn sigma_at(h: f32, xz: vec2<f32>, detail: f32) -> f32 {
    return cloud_sigma(h, xz, detail, params, base_tex, detail_tex, weather_tex, smp_repeat);
}

/// Optical depth toward the light from the point `off` (offset from the camera),
/// m⁻¹·m: six samples in a cone, base shape only. Segment lengths 30, 60, 120,
/// 240, 480 m and a far sample at 1800 m covering 1740 m. Each sample sits at a
/// fixed golden-angle azimuth around the light direction (Schneider's fixed cone
/// kernel): the lighting is a smooth function of position, so pixels do not
/// carry independent noise.
fn light_optical_depth(off: vec3<f32>, l: vec3<f32>, cam: vec3<f32>) -> f32 {
    var dist = array<f32, 6>(15.0, 60.0, 150.0, 330.0, 690.0, 1800.0);
    var len = array<f32, 6>(30.0, 60.0, 120.0, 240.0, 480.0, 1740.0);
    let helper = select(vec3<f32>(0.0, 1.0, 0.0), vec3<f32>(1.0, 0.0, 0.0), abs(l.y) > 0.99);
    let e1 = normalize(cross(l, helper));
    let e2 = cross(l, e1);
    var tau = 0.0;
    for (var j = 0u; j < 6u; j += 1u) {
        let phi = 2.39996323 * f32(j + 1u);
        let perp = cos(phi) * e1 + sin(phi) * e2;
        let o = off + l * dist[j] + perp * (CONE_TAN * dist[j]);
        let h = altitude(o.xz, cam.y + o.y, params.camera.w);
        tau += sigma_at(h, cam.xz + o.xz, 0.0) * len[j];
    }
    return tau;
}

/// March the layer from `t0` to `t1` (metres) in about `steps` primary steps,
/// the samples offset by `jitter` of a step.
fn integrate(
    cam: vec3<f32>,
    dir: vec3<f32>,
    t0: f32,
    t1: f32,
    jitter: f32,
    steps: u32,
) -> Integrated {
    let big_r = params.camera.w;
    let h_b = params.layer.x;
    let h_t = params.layer.y;
    let l = params.light.xyz;
    let n = f32(steps);
    let dt = max(clamp((t1 - t0) / n, 25.0, 400.0), (t1 - t0) / (1.5 * n));
    let cos_t = dot(dir, l);
    let detail = detail_weight(dt, params.noise.y);
    // C2 replaces this with the sky's irradiance; C1 needs *some* light in the
    // shadow side or the shape is unreadable.
    let ambient_gain = 0.02;

    var radiance = vec3<f32>(0.0);
    var transmittance = 1.0;
    var t = t0 + dt * jitter;
    var coarse = true;
    var empty = 0u;
    var resume = t;
    let max_iterations = steps * 3u;
    for (var it = 0u; it < max_iterations; it += 1u) {
        if t >= t1 || transmittance < MIN_TRANSMITTANCE {
            break;
        }
        let off = dir * t;
        let h = altitude(off.xz, cam.y + off.y, big_r);
        let xz = cam.xz + off.xz;
        if coarse {
            if coarse_shape(h, xz, params, base_tex, weather_tex, smp_repeat) > 0.0 {
                coarse = false;
                empty = 0u;
                t = max(t - 2.0 * dt, resume);
            } else {
                t += 2.0 * dt;
            }
            continue;
        }
        let sigma = sigma_at(h, xz, detail);
        if sigma <= 0.0 {
            empty += 1u;
            if empty >= EMPTY_LIMIT {
                coarse = true;
                resume = t + dt;
            }
            t += dt;
            continue;
        }
        empty = 0u;
        // Each sample stands for the segment of length dt it starts (the jitter
        // shifts the sample inside its segment, not the segment).
        let seg = clamp(t1 - (t - dt * jitter), 0.0, dt);
        let step_t = exp(-sigma * seg);

        let f = (h - h_b) / (h_t - h_b);
        let mu_s = (dot(off.xz, l.xz) + (big_r + cam.y + off.y) * l.y) / (big_r + h);
        let sun = light_at(transmittance_lut, smp_clamp, h, mu_s, params);
        let tau_l = light_optical_depth(off, l, cam);
        let ambient = params.light_e.xyz
            * ambient_gain
            * (AMBIENT_BOTTOM + (1.0 - AMBIENT_BOTTOM) * clamp(f, 0.0, 1.0));
        let scatter = sun * exp(-tau_l) * phase(cos_t) + ambient;
        radiance += transmittance * scatter * (1.0 - step_t);
        transmittance *= step_t;
        t += dt;
    }
    // A march that stopped on its transmittance floor met an opaque cloud: the
    // rest of the column is assumed to scatter like what was met.
    if transmittance < MIN_TRANSMITTANCE {
        radiance /= 1.0 - transmittance;
        transmittance = 0.0;
    }
    return Integrated(radiance, transmittance);
}

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

    var out = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    if shell.y > shell.x {
        // A surface in front of the shell hides the cloud on this ray (the
        // composite discards it anyway; skipping keeps the fresh field
        // consistent for the resolve).
        let depth = textureLoad(depth_tex, vec2<i32>(id.xy * 2u), 0);
        let hidden = depth > 0.0
            && scene_distance(uv, depth, view.world_from_clip, ray, params.units.x) < shell.x;
        if !hidden {
            if params.misc.x > 0.5 {
                // Debug shell: a warm white with per-texel jitter so temporal
                // reprojection is visible by eye (a flat field hides it).
                let jitter = mix(0.75, 1.25, hash31(vec3<i32>(vec2<i32>(id.xy), 0i)));
                let lum = params.misc.y * jitter;
                out = vec4<f32>(vec3<f32>(1.0, 0.9, 0.75) * lum, 0.0);
            } else {
                let jitter = hash31(vec3<i32>(vec2<i32>(id.xy), i32(params.counts.x)));
                let r = integrate(cam, dir, shell.x, min(shell.y, MAX_MARCH_M), jitter, params.counts.y);
                out = vec4<f32>(r.radiance, r.transmittance);
            }
        }
    }
    textureStore(march_out, vec2<i32>(id.xy), out);
}
