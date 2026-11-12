// clouds_march — the primary ray march at half resolution, with the dusk
// lighting model.
//
// Every texel is marched every frame (no Bayer slot): the fresh field is
// complete, which lets the resolve clamp against it directly. A coarse pass
// skips empty air in 2·dt steps and drops to fine steps when the base shape is
// non-zero, so most of the cost is spent inside cloud.
//
// Lighting: single scattering with the dual-lobe phase and Beer-powder, plus
// Hillaire's three analytic multiple-scattering octaves (which is what keeps a
// 2.6°-sun cloud from going black), an ambient source in the colour of the
// environment cube averaged over the upper hemisphere, aerial perspective from
// Bevy's aerial-view LUT, and a far veil so the horizon is not a hard wall at
// the march limit.
//
// Output: premultiplied (L, T) in UNexposed luminance. The composite applies
// the view exposure, so the sky's position perturbation never pollutes the
// history.

import bevy_render::view::View;
import bevy_elements::noise::hash31;
import no_one_looks_up::plugins::clouds::common::{
    CloudParams, MAX_MARCH_M, aerial_in_scatter, air_transmittance, altitude, ambient_light,
    cloud_sigma, coarse_shape, detail_weight, is_orthographic, light_at, multi_scatter, phase,
    scene_distance, shell_range, view_ray,
};

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var<uniform> params: CloudParams;
@group(0) @binding(2) var base_tex: texture_3d<f32>;
@group(0) @binding(3) var detail_tex: texture_3d<f32>;
@group(0) @binding(4) var weather_tex: texture_2d<f32>;
@group(0) @binding(5) var transmittance_lut: texture_2d<f32>;
@group(0) @binding(6) var aerial_lut: texture_3d<f32>;
@group(0) @binding(7) var env_cube: texture_cube<f32>;
@group(0) @binding(8) var depth_tex: texture_depth_2d;
@group(0) @binding(9) var smp_repeat: sampler;
@group(0) @binding(10) var smp_clamp: sampler;
@group(0) @binding(11) var march_out: texture_storage_2d<rgba16float, write>;

/// Consecutive empty fine samples after which the march returns to coarse
/// steps (Nubis).
const EMPTY_LIMIT: u32 = 10u;
/// Transmittance below which the march stops (Schneider's alpha → 1).
const MIN_TRANSMITTANCE: f32 = 0.01;
/// Tangent of the light-sample cone half angle.
const CONE_TAN: f32 = 0.1;
/// Bottom weight of the ambient gradient (the ground's bounce into the base).
const AMBIENT_BOTTOM: f32 = 0.45;
/// Upper-hemisphere taps of the environment cube.
const SKY_SAMPLES: u32 = 16;
const LUMA: vec3<f32> = vec3<f32>(0.2126, 0.7152, 0.0722);
/// Distance at which cloud extinction starts to fade toward the march limit, m.
const FADE_START_M: f32 = 12000.0;
/// Path length through the layer past `FADE_START_M` over which the far field
/// builds up to its mean cover, m: a few cloud widths, so a full deck does not
/// let the sky through at the horizon.
const FAR_LENGTH_M: f32 = 4000.0;
/// Optical depth of the far cloud toward the light along the vertical.
const FAR_TAU: f32 = 5.0;
/// Cover over which the far field turns from broken cloud to a uniform deck,
/// whose light path slants with the light.
const DECK_COVER_FROM: f32 = 0.6;
const DECK_COVER_TO: f32 = 0.95;
/// Lowest sine of the light's elevation the deck's slanted light path is built
/// for.
const MIN_LIGHT_SINE: f32 = 0.1;
/// Ray elevation (sine) below the horizon over which the far veil fades out.
const HORIZON_BAND: f32 = 0.03;

/// A marched ray: premultiplied scattered luminance and transmittance, and the
/// transmittance-weighted sum of the sample depths (for aerial perspective).
struct Integrated {
    radiance: vec3<f32>,
    transmittance: f32,
    depth_sum: f32,
    depth_weight: f32,
}

/// The sky's irradiance on a horizontal surface over `π`, `E/π = 2/N · Σ cosθ
/// L` — the ambient the cloud volume is lit by, as an equivalent radiance.
///
/// Spherical Fibonacci taps uniform in solid angle over the upper hemisphere,
/// cosine weighted (the zenith counts, the horizon is damped): a dusk sky's
/// bright horizon glow still warms the cloud's shadow side, without the whole
/// cloud glowing like a daytime cloud over a twilight sky.
///
/// The cube is Bevy's atmosphere-only environment map (the crate regenerates it
/// every frame), so the colour is the sky's own.
fn sky_irradiance() -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    for (var i = 0u; i < SKY_SAMPLES; i += 1u) {
        let y = (f32(i) + 0.5) / f32(SKY_SAMPLES);
        let r = sqrt(max(1.0 - y * y, 0.0));
        let phi = 2.3999632 * f32(i);
        let d = vec3<f32>(r * cos(phi), y, -r * sin(phi));
        sum += y * textureSampleLevel(env_cube, smp_clamp, d, 0.0).rgb;
    }
    return 2.0 * sum / f32(SKY_SAMPLES);
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

/// Radiance of cloud lit by `sun` through a light path of optical depth `tau_l`,
/// seen at angle `cos_t` to the light: single scattering with the dual-lobe
/// phase and the powder darkening near the lit surface, plus the multiple
/// octaves, plus the ambient source.
fn cloud_scatter(
    sun: vec3<f32>,
    tau_l: f32,
    cos_t: f32,
    powder_weight: f32,
    ambient: vec3<f32>,
) -> vec3<f32> {
    let weight = clamp(powder_weight * params.tuning.z, 0.0, 1.0);
    let powder = mix(1.0, 1.0 - exp(-2.0 * tau_l), weight);
    let bounced = multi_scatter(tau_l, cos_t, params.tuning.w);
    return sun * (powder * exp(-tau_l) * phase(cos_t) + bounced) + ambient;
}

/// March the layer from `t0` to `t1` (metres) in about `steps` primary steps,
/// the samples offset by `jitter` of a step, the ambient in the colour of the
/// sky irradiance `sky_e`.
fn integrate(
    cam: vec3<f32>,
    dir: vec3<f32>,
    t0: f32,
    t1: f32,
    jitter: f32,
    sky_e: vec3<f32>,
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
    let powder_weight = clamp(0.5 - 0.5 * cos_t, 0.0, 1.0);
    // The ambient's luminance at full sky, `E/π` (a cloud under the sky
    // reflects about what a horizontal white surface would see); its colour is
    // the sky's own. The sun-derived floor keeps the shadow side readable on
    // the frames before Bevy has generated the environment cube.
    let ambient_top = max(
        dot(sky_e, LUMA),
        dot(params.light_e.xyz, LUMA) * 0.004,
    ) * params.tuning.x;

    var radiance = vec3<f32>(0.0);
    var transmittance = 1.0;
    var depth_sum = 0.0;
    var depth_weight = 0.0;
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
        var sigma = sigma_at(h, xz, detail);
        // Distant cloud thins out with distance instead of ending in a hard
        // wall at the march limit; the far veil takes over behind it.
        sigma *= 1.0 - smoothstep(FADE_START_M, MAX_MARCH_M, t);
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
        let ambient = ambient_light(
            sky_e,
            ambient_top * (AMBIENT_BOTTOM + (1.0 - AMBIENT_BOTTOM) * clamp(f, 0.0, 1.0)),
        );
        let scatter = cloud_scatter(sun, tau_l, cos_t, powder_weight, ambient);
        radiance += transmittance * scatter * (1.0 - step_t);
        depth_sum += transmittance * t;
        depth_weight += transmittance;
        transmittance *= step_t;
        t += dt;
    }
    // A march that stopped on its transmittance floor met an opaque cloud: the
    // rest of the column is assumed to scatter like what was met, and nothing
    // behind shows through (Schneider & Vos 2015: alpha 1 at the exit).
    if transmittance < MIN_TRANSMITTANCE {
        radiance /= 1.0 - transmittance;
        transmittance = 0.0;
    }
    return Integrated(radiance, transmittance, depth_sum, depth_weight);
}

/// The cloud beyond the march limit: a uniform veil of the layer's mean cover
/// over the path through the layer past `FADE_START_M`, lit as the marched base
/// is lit where the march fades and seen through the air to its own distance.
/// Premultiplied luminance (rgb) and transmittance (a). Smooth in the ray, so it
/// cannot alias.
fn far_veil(
    dir: vec3<f32>,
    shell: vec2<f32>,
    o_r: f32,
    air_t: vec3<f32>,
    air_in: vec3<f32>,
    sky_e: vec3<f32>,
) -> vec4<f32> {
    let t_start = max(shell.x, FADE_START_M);
    let length = shell.y - t_start;
    let above = smoothstep(-HORIZON_BAND, 0.0, dir.y);
    let a = params.layer.w * (1.0 - exp(-max(length, 0.0) / FAR_LENGTH_M)) * above;
    if a <= 0.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let l = params.light.xyz;
    // What the march sees at grazing angles is the base of the cloud, where the
    // ambient gradient is `AMBIENT_BOTTOM`. A uniform deck lets the light in
    // along the slant of its column; a broken layer lights its sides through
    // about the column depth.
    let h_b = params.layer.x;
    let off = dir * t_start;
    let mu_s = (dot(off.xz, l.xz) + (o_r + off.y) * l.y) / (params.camera.w + h_b);
    let sun = light_at(transmittance_lut, smp_clamp, h_b, mu_s, params);
    let deck = smoothstep(DECK_COVER_FROM, DECK_COVER_TO, params.layer.w);
    let tau_l = FAR_TAU * mix(1.0, 1.0 / max(l.y, MIN_LIGHT_SINE), deck);
    let cos_t = dot(dir, l);
    let ambient = ambient_light(
        sky_e,
        max(dot(sky_e, LUMA), dot(params.light_e.xyz, LUMA) * 0.004)
            * AMBIENT_BOTTOM
            * params.tuning.x,
    );
    let radiance = cloud_scatter(sun, tau_l, cos_t, clamp(0.5 - 0.5 * cos_t, 0.0, 1.0), ambient);
    return vec4<f32>(a * (radiance * air_t + air_in), 1.0 - a);
}

/// The marched cloud seen through the air to its mean depth, over the far field
/// `veil` behind it: premultiplied luminance and transmittance.
fn finish_ray(r: Integrated, air_t: vec3<f32>, air_in: vec3<f32>, veil: vec4<f32>) -> vec4<f32> {
    let rgb = r.radiance * air_t + air_in * (1.0 - r.transmittance);
    // The composite blends `src.rgb + dst.rgb · src.a`, so the output alpha is
    // the TRANSMITTANCE, not the coverage: a clear ray leaves the sky alone.
    return vec4<f32>(rgb + r.transmittance * veil.rgb, r.transmittance * veil.a);
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
                let sky_e = sky_irradiance();
                let jitter = hash31(vec3<i32>(vec2<i32>(id.xy), i32(params.counts.x)));
                let r = integrate(
                    cam,
                    dir,
                    shell.x,
                    min(shell.y, MAX_MARCH_M),
                    jitter,
                    sky_e,
                    params.counts.y,
                );
                // Aerial perspective is applied at the cloud's mean depth (a
                // clear ray has nothing to composite: Bevy's sky already has
                // its own).
                let path = select(shell.x, r.depth_sum / max(r.depth_weight, 1e-6), r.depth_weight > 0.0);
                let o_r = big_r + cam.y;
                let air_t = air_transmittance(dir, path, o_r, transmittance_lut, smp_clamp, params);
                let air_in = aerial_in_scatter(
                    uv,
                    path,
                    air_t,
                    aerial_lut,
                    smp_clamp,
                    params,
                    sky_e,
                );
                let veil = far_veil(dir, shell, o_r, air_t, air_in, sky_e);
                out = finish_ray(r, air_t, air_in, veil);
            }
        }
    }
    textureStore(march_out, vec2<i32>(id.xy), out);
}
