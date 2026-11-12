// The cloud weather map: a camera-centred, 16 km periodic coverage field,
// thresholded by a per-frame histogram so the area fraction of cloud equals the
// selected kind's cover.
//
//   cloud_histogram_clear   zero the 256 bins
//   cloud_field             histogram of the field, 8 bit
//   cloud_threshold         scan the bins from the top to the quantile 1 − cover
//   cloud_weather           R coverage, A dome
//
// The window is 16 km rather than the vendored 32 km because the camera looks
// up 22.55–60° into a 0.8–3.2 km shell: all visible cloud is within ~10 km.
// The field is periodic over the window, so its histogram does not depend on
// where the window sits and the map's repeat has no seam.

import bevy_elements::noise::fbm3_periodic;
import no_one_looks_up::plugins::clouds::common::CloudParams;

@group(0) @binding(0) var<uniform> params: CloudParams;
@group(0) @binding(1) var<storage, read_write> histogram: array<atomic<u32>, 256>;
@group(0) @binding(2) var<storage, read_write> threshold: array<f32, 4>;
@group(0) @binding(3) var weather_out: texture_storage_2d<rgba8unorm, write>;

var<workgroup> local_bins: array<atomic<u32>, 256>;

/// Texels per side of the weather map; mirrors `config::WEATHER_TEXELS`.
const TEXELS: u32 = 256u;
/// Soft edge of the coverage threshold, field units: the coverage ramps from 1
/// to 0 over this band around the threshold, so blob edges are soft.
const SOFT_EDGE: f32 = 0.18;
/// Width over which the dome factor rises from 0 at the cloud edge to 1 deep in
/// a blob, field units. 0.30 rather than the upstream 0.37: the upstream value
/// keeps full height across most of a blob, so the dome read as a flat disc
/// with a bump rather than a pile.
const DOME_WIDTH: f32 = 0.30;
/// Largest |fbm| of the 3-octave periodic Perlin field on its z slice, which
/// maps the noise onto [0, 1] (octave count and gain only, so it does not
/// depend on the frequency).
const FIELD_MAX: f32 = 0.385;

/// The coverage noise at a texel, [0, 1]: 3 octaves of 2D Perlin on a lattice
/// that wraps every 16 km (4 base cells, wavelengths 4 / 2 / 1 km), evaluated at
/// the texel's world position minus the cloud drift.
fn coverage_field(texel: vec2<u32>) -> f32 {
    let xz = params.weather.xy + (vec2<f32>(texel) + 0.5) * (params.weather.z / f32(TEXELS));
    let q = (xz - params.drift.xy) / 4000.0;
    let n = fbm3_periodic(vec3<f32>(q.x, q.y, 0.31), 4, 3);
    return clamp(0.5 + 0.5 * n / FIELD_MAX, 0.0, 1.0);
}

@compute @workgroup_size(256)
fn cloud_histogram_clear(@builtin(local_invocation_index) i: u32) {
    atomicStore(&histogram[i], 0u);
}

@compute @workgroup_size(8, 8, 1)
fn cloud_field(
    @builtin(global_invocation_id) id: vec3<u32>,
    @builtin(local_invocation_index) lane: u32,
) {
    if all(id.xy < vec2<u32>(TEXELS)) {
        let bin = min(u32(coverage_field(id.xy) * 256.0), 255u);
        atomicAdd(&local_bins[bin], 1u);
    }
    workgroupBarrier();
    for (var i = lane; i < 256u; i += 64u) {
        let count = atomicLoad(&local_bins[i]);
        if count > 0u {
            atomicAdd(&histogram[i], count);
        }
    }
}

@compute @workgroup_size(1)
fn cloud_threshold() {
    let target_count = params.layer.w * f32(TEXELS * TEXELS);
    var cumulative = 0.0;
    var theta = 256.0;
    for (var k = 255; k >= 0; k -= 1) {
        let count = f32(atomicLoad(&histogram[k]));
        if cumulative + count >= target_count {
            // Bin k spans field values [k, k + 1) / 256; the mass fills it from
            // the top.
            theta = f32(k) + 1.0 - (target_count - cumulative) / max(count, 1.0);
            break;
        }
        cumulative += count;
    }
    threshold[0] = theta;
}

@compute @workgroup_size(8, 8, 1)
fn cloud_weather(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= vec2<u32>(TEXELS)) {
        return;
    }
    let cover = params.layer.w;
    let field = coverage_field(id.xy) * 256.0;
    let edge = (field - threshold[0]) / (SOFT_EDGE * 256.0);
    var coverage = clamp(0.5 + edge, 0.0, 1.0);
    if cover >= 0.995 {
        coverage = 1.0;
    } else if cover <= 0.005 {
        coverage = 0.0;
    }
    let dome = clamp((field - threshold[0]) / (DOME_WIDTH * 256.0), 0.0, 1.0);
    textureStore(weather_out, vec2<i32>(id.xy), vec4<f32>(coverage, 0.0, 0.0, dome));
}
