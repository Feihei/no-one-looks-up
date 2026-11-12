// cloud_noise_base / cloud_noise_detail — the cloud noise volumes, generated
// once by compute (the recipe of Hillaire's TileableVolumeNoise, which realises
// Schneider 2015). Every channel tiles with the texture period.
//
//   base   256³: R Perlin-Worley, G/B/A Worley fBm at 8/16/32/64-cell octaves
//   detail  64³: R/G/B Worley fBm, the finest octave 16 m across
//
// The volumes are 8 m per texel (the vendored layer's are 32 m), which is the
// direct lever on the blocky silhouette: the smallest billows the cloud can
// carry are 2–4× finer. The base volume is produced in `NOISE_PASSES` z slabs
// over as many frames (`params.counts.w` is the first slice of this slab).

import bevy_elements::noise::{fbm3_periodic, worley3};
import no_one_looks_up::plugins::clouds::common::CloudParams;

@group(0) @binding(0) var<uniform> params: CloudParams;
@group(0) @binding(1) var base_out: texture_storage_3d<rgba8unorm, write>;
@group(0) @binding(2) var detail_out: texture_storage_3d<rgba8unorm, write>;

/// Texels per side of the base volume; mirrors `config::NOISE_BASE_TEXELS`.
const BASE_TEXELS: u32 = 256u;
/// Texels per side of the detail volume; mirrors `config::NOISE_DETAIL_TEXELS`.
const DETAIL_TEXELS: u32 = 64u;
/// Largest |fbm| of the 3-octave periodic Perlin sum (it depends on the octave
/// count and gain only, so the vendored constant holds), which maps the noise
/// onto [0, 1].
const PERLIN_FBM_MAX: f32 = 0.55;

/// Inverted Worley noise with `k` cells across the tile; `x` is in [0, 1).
fn inverted_worley(x: vec3<f32>, k: i32) -> f32 {
    return 1.0 - min(worley3(x * f32(k), k), 1.0);
}

@compute @workgroup_size(4, 4, 4)
fn cloud_noise_base(@builtin(global_invocation_id) id: vec3<u32>) {
    let z = id.z + params.counts.w;
    if id.x >= BASE_TEXELS || id.y >= BASE_TEXELS || z >= BASE_TEXELS {
        return;
    }
    let texel = vec3<u32>(id.x, id.y, z);
    let uvw = (vec3<f32>(texel) + 0.5) / f32(BASE_TEXELS);
    // Cell sizes 256 / 128 / 64 / 32 / 16 m.
    let w8 = inverted_worley(uvw, 8);
    let w16 = inverted_worley(uvw, 16);
    let w32 = inverted_worley(uvw, 32);
    let w64 = inverted_worley(uvw, 64);
    let w128 = inverted_worley(uvw, 128);
    let perlin = clamp(0.5 + 0.5 * fbm3_periodic(uvw * 8.0, 8, 3) / PERLIN_FBM_MAX, 0.0, 1.0);
    // The Perlin-Worley hybrid: Worley's cell edges dilated by Perlin's smooth
    // field, so the cloud's core keeps cellular billows and its outline is
    // smooth.
    let dilated = 0.625 * w8 + 0.25 * w32 + 0.125 * w64;
    let r = dilated + perlin * (1.0 - dilated);
    let g = 0.625 * w8 + 0.25 * w16 + 0.125 * w32;
    let b = 0.625 * w16 + 0.25 * w32 + 0.125 * w64;
    let a = 0.75 * w32 + 0.25 * w128;
    textureStore(base_out, vec3<i32>(texel), vec4<f32>(r, g, b, a));
}

@compute @workgroup_size(4, 4, 4)
fn cloud_noise_detail(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id >= vec3<u32>(DETAIL_TEXELS)) {
        return;
    }
    let uvw = (vec3<f32>(id) + 0.5) / f32(DETAIL_TEXELS);
    // Cell sizes 256 / 128 / 64 / 32 / 16 m.
    let w2 = inverted_worley(uvw, 2);
    let w4 = inverted_worley(uvw, 4);
    let w8 = inverted_worley(uvw, 8);
    let w16 = inverted_worley(uvw, 16);
    let w32 = inverted_worley(uvw, 32);
    let r = 0.625 * w2 + 0.25 * w4 + 0.125 * w8;
    let g = 0.625 * w4 + 0.25 * w8 + 0.125 * w16;
    // The finest octave is weighted heavily here rather than at 0.125: a
    // frayed edge needs the 16 m cells to move the erosion threshold, and the
    // vendored weights buried them under the coarse ones (which is why its
    // silhouettes read as smooth blobs).
    let b = 0.5 * w16 + 0.5 * w32;
    textureStore(detail_out, vec3<i32>(id), vec4<f32>(r, g, b, 1.0));
}
