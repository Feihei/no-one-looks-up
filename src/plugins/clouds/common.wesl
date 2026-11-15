// no_one_looks_up::plugins::clouds::common — the cloud model shared by every
// cloud kernel: the uniform, shell geometry, the density model and the
// atmosphere lookups. No bindings: each kernel declares its own and passes the
// textures in.
//
// Adapted for the game's ONE golden-hour cumulus layer (adr-0007). The shell
// geometry and the Bevy-atmosphere lookups (transmittance_uv,
// sample_transmittance, planet_visibility, light_at, view_ray, scene_distance,
// sphere_roots, shell_range, ground_before_layer, is_orthographic, altitude)
// are a copy of the vendored bevy_elements::clouds::common (they are
// `pub(crate)` there); the tripwire test `common_wesl_carries_the_copied_math`
// pins them. The density model is the game's own: one cumulus family whose
// kinds differ in gradient, extinction, dome weight and erosion, with the
// vendored layer's validated dome shaping (0.62 exponent, ±35 % per-cell
// jitter, DOME_WIDTH 0.30 in the weather map).
//
// Geometry: one spherical shell about a planet centre straight below the
// camera. A point is described by its horizontal offset `rel_xz` from the
// camera and its world height `y`; its altitude above the surface is computed
// without forming radii of 6.36e6 m in f32.

/// Mirrors the Rust `CloudsParams` (`params.rs`). Every field is a vec4 so the
/// layouts agree.
struct CloudParams {
    /// xyz: camera world position, m. w: planet radius R, m.
    camera: vec4<f32>,
    /// x: atmosphere outer radius, m. y: aerial-view LUT range, m. z: aerial-view LUT slices.
    /// w: the night sky's background radiance, cd/m².
    atmo: vec4<f32>,
    /// x: layer base altitude, m. y: layer top altitude, m. z: cloud type 0–1. w: cloud cover.
    layer: vec4<f32>,
    /// xyz: unit direction toward the sun. w: unused.
    light: vec4<f32>,
    /// xyz: the sun's illuminance outside the atmosphere, lx. w: unused (C2's ambient source).
    light_e: vec4<f32>,
    /// xy: drift of the weather field, m (wrapped to its tile). zw: drift of the base noise, m.
    drift: vec4<f32>,
    /// xy: drift of the detail noise, m. zw: unit wind direction (XZ).
    drift_detail: vec4<f32>,
    /// xy: weather map origin, m. z: its extent, m. w: unused.
    weather: vec4<f32>,
    /// x: frame counter. y: primary march steps Q. z: light samples. w: first z slice of this
    /// frame's noise slab.
    counts: vec4<u32>,
    /// xy: full resolution. zw: march resolution.
    sizes: vec4<u32>,
    /// x: debug mode (0 off, 1 uniform shell of luminance y). y: debug shell luminance.
    /// z: detail-volume weight in the density (0 on `Low`, 1 otherwise). w: 1 on the first
    /// frame(s) of a history (no valid history).
    misc: vec4<f32>,
    /// x: world units per metre (`ElementsUnits`). Everything above is in metres; Bevy's view
    /// matrices and depth are in world units, so the shaders divide what they take from them by
    /// this and multiply what they hand back.
    units: vec4<f32>,
    /// The height gradient quadruple (a0, a1, a2, a3) of the selected kind.
    grad: vec4<f32>,
    /// x: peak extinction, m⁻¹. y: cloud-side erosion strength. z: weight of the dome shaping
    /// (0 a flat deck, 1 cumulus). w: unused.
    shape: vec4<f32>,
    /// x: base noise tile, m. y: detail noise tile, m. z: wind skew per unit height, m.
    /// w: unused.
    noise: vec4<f32>,
    /// x: ambient brightness fuse (1 neutral). y: aerial in-scatter fuse (1 neutral).
    /// z: powder strength (1 neutral). w: multiple-scattering strength (1 neutral).
    tuning: vec4<f32>,
}

const PI: f32 = 3.14159265358979;
/// Farthest distance the march goes, m. The camera looks up 22.55–60° into a
/// shell at 0.8–3.2 km, so visible cloud is within ~10 km; 16 km leaves slack
/// for the far veil (C2) without sampling the whole aerial LUT range.
const MAX_MARCH_M: f32 = 16000.0;
/// Large number standing for "no surface": the sky.
const NO_SURFACE: f32 = 1.0e30;

// ---------------------------------------------------------------------------
// Density model (cumulus, one family).
// ---------------------------------------------------------------------------

/// The dome profile's exponent. 0.62 rather than sqrt: a flatter shoulder,
/// closer to a cumulus pile than to a half-ellipse.
const DOME_EXPONENT: f32 = 0.62;
/// Per-cell spread of the dome profile, ±35 %: blobs are not identical copies.
const DOME_JITTER: f32 = 0.35;
/// Flat base of a cumuliform cloud: density rises over this fraction of the
/// layer, so the condensation level is a plane.
const CUMULUS_BASE_RISE: f32 = 0.06;
/// The top rounds off over the upper part of the blob's height, from this
/// fraction of it.
const CUMULUS_TOP_ROUND: f32 = 0.25;
/// Height of a cumuliform cloud at the edge of its blob, as a fraction of the
/// layer.
const CUMULUS_EDGE_HEIGHT: f32 = 0.45;
/// Share of the cumuliform profile the cloud's core keeps, so the noise's lows
/// still open gaps inside a coverage blob and it breaks into separate clouds.
const CORE_PROFILE: f32 = 0.75;

/// Height gradient quadruple: density ramps in between `q.x` and `q.y` and out
/// between `q.z` and `q.w`.
fn height_gradient(f: f32, q: vec4<f32>) -> f32 {
    let up = clamp((f - q.x) / max(q.y - q.x, 1e-4), 0.0, 1.0);
    let down = 1.0 - clamp((f - q.z) / max(q.w - q.z, 1e-4), 0.0, 1.0);
    return up * down;
}

/// The rounded top of a cumuliform cloud at height fraction `f` with the dome
/// factor `dome` (the weather map's A channel, 0 at the edge of a blob, 1 deep
/// inside): full up to the upper part of the blob's height `s`, rounding off
/// toward it. C¹, so neither the corners of the gradient nor the dome texels'
/// kinks crease the cloud.
///
/// `weight` is the kind's dome weight: 0 makes `s` the layer top, so a deck is
/// a slab of constant thickness instead of a field of domes. `cell` (the base
/// noise's Worley channel) jitters the profile per cell, ±35 %.
fn cumulus_envelope(f: f32, dome: f32, cell: f32, weight: f32) -> f32 {
    let profile = pow(dome, DOME_EXPONENT) * mix(1.0 - DOME_JITTER, 1.0 + DOME_JITTER, cell);
    let shaped = mix(CUMULUS_EDGE_HEIGHT, 1.0, clamp(profile, 0.0, 1.0));
    let s = mix(1.0, shaped, clamp(weight, 0.0, 1.0));
    return 1.0 - smoothstep(s * CUMULUS_TOP_ROUND, s, f);
}

/// The flat base of a cumuliform cloud: a factor on the eroded density, not on
/// the profile the noise erodes, so no billow hangs below the condensation
/// level.
fn cumulus_base(f: f32) -> f32 {
    return smoothstep(0.0, CUMULUS_BASE_RISE, f);
}

/// Weight of the billowy composite: wispy at the base, billowy above it (Nubis
/// Evolved's `noise_height_blend`).
fn billow_weight(f: f32) -> f32 {
    return smoothstep(0.05, 0.25, f);
}

/// Nubis' value erosion `saturate(noise − (1 − d))`, normalised to reach 1 where
/// the noise is 1 and raised to the edge exponent ½: 0 wherever the noise is
/// below `1 − d`, and a cloud's core is as dense as its kind allows however
/// thin the profile that carved it.
fn value_erosion(noise: f32, d: f32) -> f32 {
    if d <= 0.0 {
        return 0.0;
    }
    return sqrt(clamp((noise - (1.0 - d)) / d, 0.0, 1.0));
}

/// The dimensional profile of a cumuliform cloud: the rounded envelope times the
/// kind's height gradient times the coverage `r` times the convective cells
/// `cell` (a coverage blob is a field of separate thermals, not one mass). The
/// gaps close as the cover reaches 1 (`cover⁴`): an overcast sky has no holes,
/// its billows pack into a deck.
fn cumulus_profile(f: f32, dome: f32, r: f32, cell: f32, cover: f32, grad: vec4<f32>, dome_weight: f32) -> f32 {
    let c2 = clamp(cover, 0.0, 1.0) * clamp(cover, 0.0, 1.0);
    let closed = c2 * c2;
    let gaps = mix(CORE_PROFILE * smoothstep(0.25, 0.65, cell), 1.0, closed);
    let envelope = cumulus_envelope(f, dome, cell, dome_weight);
    return envelope * height_gradient(f, grad) * r * gaps;
}

/// The noise composite of a cumuliform cloud from the base Perlin-Worley
/// `shape`, the base Worley octaves `lf` and the detail Worley octaves `hf`:
/// wispy toward the base, billowy above.
fn noise_composite(f: f32, shape: f32, lf: f32, hf: f32) -> f32 {
    let wispy = 0.6 * shape + 0.4 * (1.0 - hf);
    let billowy = 0.5 * shape + 0.3 * lf + 0.2 * hf;
    return mix(wispy, billowy, billow_weight(f));
}

/// The largest `noise_composite` any detail value gives: the coarse march's
/// bound, so it never steps over a cloud the fine one would draw.
fn noise_bound(f: f32, shape: f32, lf: f32) -> f32 {
    return mix(0.6 * shape + 0.4, 0.5 * shape + 0.3 * lf + 0.2, billow_weight(f));
}

/// Base-noise coordinate of a (wind-leaned) point: the world XZ minus the drift
/// over the tile, and the absolute altitude over the tile.
fn base_uvw(lean: vec2<f32>, h: f32, p: CloudParams) -> vec3<f32> {
    return vec3<f32>(
        (lean.x - p.drift.z) / p.noise.x,
        h / p.noise.x,
        (lean.y - p.drift.w) / p.noise.x,
    );
}

/// Detail-noise coordinate of a (wind-leaned) point.
fn detail_uvw(lean: vec2<f32>, h: f32, p: CloudParams) -> vec3<f32> {
    return vec3<f32>(
        (lean.x - p.drift_detail.x) / p.noise.y,
        h / p.noise.y,
        (lean.y - p.drift_detail.y) / p.noise.y,
    );
}

/// How much of the detail noise a fine step of `dt` metres resolves: all of it
/// up to an eighth of the detail tile (its finest cells are a sixteenth), none
/// past a quarter.
fn detail_weight(dt: f32, tile: f32) -> f32 {
    return 1.0 - smoothstep(tile / 8.0, tile / 4.0, dt);
}

/// The base-shape term, 0 outside cloud: what the coarse march samples. It is
/// the value erosion of the largest composite the detail could give, so the
/// coarse march never steps over a cloud the fine one would draw.
fn coarse_shape(
    h: f32,
    xz: vec2<f32>,
    p: CloudParams,
    base_tex: texture_3d<f32>,
    weather_tex: texture_2d<f32>,
    smp: sampler,
) -> f32 {
    let h_b = p.layer.x;
    let f = (h - h_b) / (p.layer.y - h_b);
    if f < 0.0 || f > 1.0 {
        return 0.0;
    }
    let w = textureSampleLevel(weather_tex, smp, (xz - p.weather.xy) / p.weather.z, 0.0);
    if w.r <= 0.0 {
        return 0.0;
    }
    let lean = xz - f * p.drift_detail.zw * p.noise.z;
    let b4 = textureSampleLevel(base_tex, smp, base_uvw(lean, h, p), 0.0);
    let lf = 0.625 * b4.g + 0.25 * b4.b + 0.125 * b4.a;
    let shape = clamp((b4.r + 1.0 - lf) / (2.0 - lf), 0.0, 1.0);
    let d = cumulus_profile(f, w.a, w.r, b4.g, p.layer.w, p.grad, p.shape.z);
    // Only the sign matters to the coarse march: no division or power.
    return max(noise_bound(f, shape, lf) - (1.0 - d), 0.0);
}

/// The extinction at a point of altitude `h` above world XZ `xz`, m⁻¹. `detail`
/// (0–1) is how much of the detail noise the sample can resolve: 0 skips the tap
/// (the light march), and the primary march lowers it as its step outgrows the
/// billows, so a long step sees their mean instead of aliasing them.
fn cloud_sigma(
    h: f32,
    xz: vec2<f32>,
    detail: f32,
    p: CloudParams,
    base_tex: texture_3d<f32>,
    detail_tex: texture_3d<f32>,
    weather_tex: texture_2d<f32>,
    smp: sampler,
) -> f32 {
    let h_b = p.layer.x;
    let f = (h - h_b) / (p.layer.y - h_b);
    if f < 0.0 || f > 1.0 {
        return 0.0;
    }
    let w = textureSampleLevel(weather_tex, smp, (xz - p.weather.xy) / p.weather.z, 0.0);
    if w.r <= 0.0 {
        return 0.0;
    }
    let lean = xz - f * p.drift_detail.zw * p.noise.z;
    let b4 = textureSampleLevel(base_tex, smp, base_uvw(lean, h, p), 0.0);
    let lf = 0.625 * b4.g + 0.25 * b4.b + 0.125 * b4.a;
    let base_shape = clamp((b4.r + 1.0 - lf) / (2.0 - lf), 0.0, 1.0);
    // Without the detail tap the detail octaves take their mean.
    var raw_hf = 0.5;
    if detail > 0.0 {
        let d4 = textureSampleLevel(detail_tex, smp, detail_uvw(lean, h, p), 0.0);
        // Weighted toward the fine channels: `generate.wesl` put the 16 m cells
        // in detail.b, and a frayed edge needs them to reach the erosion.
        raw_hf = 0.45 * d4.r + 0.30 * d4.g + 0.25 * d4.b;
    }
    let hf = mix(0.5, raw_hf, detail);
    let d = cumulus_profile(f, w.a, w.r, b4.g, p.layer.w, p.grad, p.shape.z);
    var density = value_erosion(noise_composite(f, base_shape, lf, hf), d) * cumulus_base(f);
    // Cloud-side erosion: the detail noise eats the edges, which is what makes
    // a silhouette frayed instead of smooth. The wispy term is stronger at the
    // base, where a cumulus trails.
    if detail > 0.0 && density > 0.0 {
        let wispy = mix(raw_hf, 1.0 - raw_hf, clamp(10.0 * f, 0.0, 1.0));
        let lo = p.shape.y * wispy;
        density = clamp((density - lo) / (1.0 - max(lo, 1e-4)), 0.0, 1.0);
    }
    return p.shape.x * density;
}

// ---------------------------------------------------------------------------
// Shell geometry and view rays (copied from the vendored crate).
// ---------------------------------------------------------------------------

/// Altitude above the surface of the point `rel_xz` away from the camera horizontally at world
/// height `y`, m: (r² − R²) / (r + R) with r² − R² built from small terms.
fn altitude(rel_xz: vec2<f32>, y: f32, big_r: f32) -> f32 {
    let h2 = dot(rel_xz, rel_xz);
    let a = h2 + y * (2.0 * big_r + y);
    let r = sqrt(h2 + (big_r + y) * (big_r + y));
    return a / (r + big_r);
}

/// The two roots (near, far) of |o + t d| = r for a ray from radius `o_r` whose direction has
/// cosine `mu` with the local vertical. (1, −1) when the ray misses the sphere. Numerically
/// stable form (Numerical Recipes q) so a 1.5 km shell is found to centimetres in f32.
fn sphere_roots(o_r: f32, mu: f32, r: f32) -> vec2<f32> {
    let b = o_r * mu;
    let c0 = (o_r - r) * (o_r + r);
    let disc = b * b - c0;
    if disc < 0.0 {
        return vec2<f32>(1.0, -1.0);
    }
    let s = sqrt(disc);
    let q = -(b + select(-s, s, b >= 0.0));
    var t2 = 0.0;
    if abs(q) > 1e-6 {
        t2 = c0 / q;
    }
    return vec2<f32>(min(q, t2), max(q, t2));
}

/// Where a ray from the camera is inside the cloud shell: (t_enter, t_exit); empty (exit ≤
/// enter) when it never is. Not clamped to the march range.
fn shell_range(o_r: f32, mu: f32, h_b: f32, h_t: f32, big_r: f32) -> vec2<f32> {
    let top = sphere_roots(o_r, mu, big_r + h_t);
    if top.y <= 0.0 {
        return vec2<f32>(0.0, 0.0);
    }
    let t_in = max(top.x, 0.0);
    let t_out = top.y;
    let base = sphere_roots(o_r, mu, big_r + h_b);
    if base.x > base.y {
        return vec2<f32>(t_in, t_out);
    }
    // The ray passes through the hollow below the base between base.x and base.y.
    let first_end = min(t_out, base.x);
    if first_end > t_in {
        return vec2<f32>(t_in, first_end);
    }
    return vec2<f32>(max(t_in, base.y), t_out);
}

/// Distance to the planet's surface along a ray that meets it before it enters the cloud layer
/// at `shell_in` (a ray past the edge of a finite ground), or −1 when it does not. Such a ray sees
/// no cloud: `shell_range` does not know the planet and finds the layer on its far side.
fn ground_before_layer(o_r: f32, mu: f32, shell_in: f32, big_r: f32) -> f32 {
    let ground = sphere_roots(o_r, mu, big_r);
    if ground.x > 0.0 && ground.x <= ground.y && ground.x < shell_in {
        return ground.x;
    }
    return -1.0;
}

/// A view ray: its origin and unit direction.
struct ViewRay {
    origin: vec3<f32>,
    dir: vec3<f32>,
}

/// Whether `clip_from_view` is an orthographic projection: `w` is 1 everywhere (Bevy's own
/// test, `cluster_raster.wesl`).
fn is_orthographic(clip_from_view: mat4x4<f32>) -> bool {
    return clip_from_view[3].w == 1.0;
}

/// The ray through screen position `uv` (0–1, top-left origin), in metres: the view matrices
/// are in world units, `units` of them to the metre. A perspective view's starts at the eye
/// `cam` (world). An orthographic view's runs along the view direction from the pixel's point
/// on the plane through `focus` (metres) across it (Marschner & Shirley §4.3.1): without a
/// focus `focus` is the camera and the plane is its own. The direction joins the pixel's
/// points at reverse-Z depths 1 and 0.5, finite for both projections.
fn view_ray(
    uv: vec2<f32>,
    world_from_clip: mat4x4<f32>,
    cam: vec3<f32>,
    orthographic: bool,
    focus: vec3<f32>,
    units: f32,
) -> ViewRay {
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let n = world_from_clip * vec4<f32>(ndc, 1.0, 1.0);
    let on_near = n.xyz / n.w / units;
    if orthographic {
        let m = world_from_clip * vec4<f32>(ndc, 0.5, 1.0);
        let dir = normalize(m.xyz / m.w / units - on_near);
        return ViewRay(on_near + dir * dot(focus - on_near, dir), dir);
    }
    let eye = cam / units;
    return ViewRay(eye, normalize(on_near - eye));
}

/// Distance along the ray (metres) from its `origin` to the surface seen at `uv` with reverse-Z
/// `depth`, negative for a surface in front of the origin; a huge value for the sky (depth 0).
fn scene_distance(
    uv: vec2<f32>,
    depth: f32,
    world_from_clip: mat4x4<f32>,
    ray: ViewRay,
    units: f32,
) -> f32 {
    if depth <= 0.0 {
        return NO_SURFACE;
    }
    let ndc = vec2<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let p = world_from_clip * vec4<f32>(ndc, depth, 1.0);
    return dot(p.xyz / p.w / units - ray.origin, ray.dir);
}

// ---------------------------------------------------------------------------
// Phase and the atmosphere lookups (copied from the vendored crate).
// ---------------------------------------------------------------------------

/// Henyey–Greenstein phase function, sr⁻¹.
fn hg(cos_t: f32, g: f32) -> f32 {
    let g2 = g * g;
    return (1.0 - g2) / (4.0 * PI * pow(1.0 + g2 - 2.0 * g * cos_t, 1.5));
}

/// The single-scattering dual lobe: forward lobe (g = 0.8) weighted 0.7, backward lobe
/// (g = −0.5) weighted 0.3. The backward lobe is what turns a backlit rim silver.
fn phase(cos_t: f32) -> f32 {
    return 0.7 * hg(cos_t, 0.8) + 0.3 * hg(cos_t, -0.5);
}

/// The dual lobe with the forward lobe's anisotropy set to `g` (the multiple-scattering
/// octaves narrow it).
fn dual_phase(cos_t: f32, g: f32) -> f32 {
    return 0.7 * hg(cos_t, g) + 0.3 * hg(cos_t, -0.5);
}

// Hillaire's analytic multiple-scattering octaves (2020, "A Physically Based Sky, Atmosphere
// and Cloud Rendering"): the light that has bounced off the cloud twice or more, as a sum of
// three attenuated, phase-shifted copies of the single-scattering answer. It costs no extra
// samples — only the light optical depth, which single scattering already paid for.
const MS_A: f32 = 0.4;
const MS_B: f32 = 0.4;
const MS_C: f32 = 0.7;

/// Light scattered more than once, relative to the incident sun: three octaves, each down by
/// `MS_A` in amplitude, `MS_B` in optical depth and `MS_C` in forward anisotropy. This is what
/// keeps a dusk cloud from going black — at 2.6° of sun elevation a single scatter only
/// reaches the tops, while the bounced light fills the body and gives it its internal glow.
fn multi_scatter(tau_l: f32, cos_t: f32, strength: f32) -> f32 {
    var a = MS_A;
    var b = MS_B;
    var c = MS_C;
    var sum = 0.0;
    for (var octave = 0; octave < 3; octave += 1) {
        sum += a * exp(-tau_l * b) * dual_phase(cos_t, 0.8 * c);
        a *= MS_A;
        b *= MS_B;
        c *= MS_C;
    }
    return sum * strength;
}

/// The ambient source of luminance `s` in the colour of the sky's irradiance `sky_e`.
fn ambient_light(sky_e: vec3<f32>, s: f32) -> vec3<f32> {
    let lum = dot(sky_e, vec3<f32>(0.2126, 0.7152, 0.0722));
    if !(lum > 1e-12) {
        return vec3<f32>(s);
    }
    return sky_e * (s / lum);
}

/// Transmittance of the air from the camera at radius `o_r` to `depth` along `dir`.
///
/// The LUT holds the transmittance to the top of the atmosphere, which a descending ray never
/// reaches (it meets the ground): the air between two points is then taken along the reversed
/// ray, which leaves the atmosphere through the camera (Bruneton's `GetTransmittance`).
fn air_transmittance(
    dir: vec3<f32>,
    depth: f32,
    o_r: f32,
    lut: texture_2d<f32>,
    smp: sampler,
    p: CloudParams,
) -> vec3<f32> {
    let r_d = sqrt(max(o_r * o_r + 2.0 * o_r * depth * dir.y + depth * depth, 1.0));
    let mu_d = (o_r * dir.y + depth) / r_d;
    if dir.y < 0.0 {
        let t_from_far = sample_transmittance(lut, smp, r_d, -mu_d, p);
        let t_from_cam = sample_transmittance(lut, smp, o_r, -dir.y, p);
        return min(t_from_far / max(t_from_cam, vec3<f32>(1e-6)), vec3<f32>(1.0));
    }
    let t_cam = sample_transmittance(lut, smp, o_r, dir.y, p);
    let t_far = sample_transmittance(lut, smp, r_d, mu_d, p);
    return min(t_cam / max(t_far, vec3<f32>(1e-6)), vec3<f32>(1.0));
}

/// The air's in-scatter along the screen position `uv` to `depth`, luminance: Bevy's
/// aerial-view LUT holds the beam's in-scatter (the sun light that carries only the cloud's
/// beam), and the rest is the air lit by the cloud-scattered diffuse light, `(1 − T) · L` for
/// the uniform sky radiance `L`.
fn aerial_in_scatter(
    uv: vec2<f32>,
    depth: f32,
    t_ap: vec3<f32>,
    lut: texture_3d<f32>,
    smp: sampler,
    p: CloudParams,
    radiance: vec3<f32>,
) -> vec3<f32> {
    let slices = p.atmo.z;
    let range = p.atmo.y;
    let slice = clamp(depth / range - 0.5 / slices, 0.0, 1.0);
    let lut_value = textureSampleLevel(lut, smp, vec3<f32>(uv, slice), 0.0);
    let beam = exp(lut_value.rgb) * clamp(depth / (range / slices), 0.0, 1.0);
    return (beam + (vec3<f32>(1.0) - t_ap) * radiance) * p.tuning.y;
}

/// Visibility of the light from a point of altitude `h` whose local vertical makes cosine `mu`
/// with the light: 0 below the planet's horizon, blended over ±0.0044 rad (half a sun disc).
fn planet_visibility(h: f32, mu: f32, big_r: f32) -> f32 {
    let mu_h = -sqrt(max(h * (2.0 * big_r + h), 0.0)) / (big_r + h);
    return smoothstep(mu_h - 0.0044, mu_h + 0.0044, mu);
}

/// UV in Bevy's transmittance LUT for radius `r` and zenith cosine `mu` (Bruneton's mapping,
/// derived from its definition; the LUT is a function of the medium only).
fn transmittance_uv(r: f32, mu: f32, ri: f32, ro: f32) -> vec2<f32> {
    let big_h = sqrt(ro * ro - ri * ri);
    let rho = sqrt(max(r * r - ri * ri, 0.0));
    let d = max(-r * mu + sqrt(max(r * r * (mu * mu - 1.0) + ro * ro, 0.0)), 0.0);
    let d_min = ro - r;
    let d_max = rho + big_h;
    return vec2<f32>((d - d_min) / (d_max - d_min), rho / big_h);
}

/// Transmittance from radius `r` along zenith cosine `mu` to the top of the atmosphere.
fn sample_transmittance(
    lut: texture_2d<f32>,
    smp: sampler,
    r: f32,
    mu: f32,
    p: CloudParams,
) -> vec3<f32> {
    return textureSampleLevel(lut, smp, transmittance_uv(r, mu, p.camera.w, p.atmo.x), 0.0).rgb;
}

/// Illuminance of the light at altitude `h` where the light makes cosine `mu` with the local
/// vertical: outside-atmosphere illuminance × atmospheric transmittance × planet visibility.
/// The golden hour locks the sun at ~2.6°, so this carries the dusk tint: a constant white
/// illuminance through the LUT comes out amber.
fn light_at(
    lut: texture_2d<f32>,
    smp: sampler,
    h: f32,
    mu: f32,
    p: CloudParams,
) -> vec3<f32> {
    let r = p.camera.w + h;
    let v = planet_visibility(h, mu, p.camera.w);
    if v <= 0.0 {
        return vec3<f32>(0.0);
    }
    return p.light_e.xyz * sample_transmittance(lut, smp, r, clamp(mu, -1.0, 1.0), p) * v;
}
