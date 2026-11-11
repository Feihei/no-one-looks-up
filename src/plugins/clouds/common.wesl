// no_one_looks_up::plugins::clouds::common — the cloud model shared by every
// cloud kernel: the uniform, shell geometry and the atmosphere lookups.
// No bindings: each kernel declares its own and passes the textures in.
//
// Adapted for the game's ONE golden-hour cumulus layer (adr-0007). The shell
// geometry and the Bevy-atmosphere lookups (transmittance_uv,
// sample_transmittance, planet_visibility, light_at, view_ray,
// scene_distance, sphere_roots, shell_range, ground_before_layer,
// is_orthographic, altitude) are a copy of the vendored
// bevy_elements::clouds::common (they are `pub(crate)` there); the tripwire
// test `common_wesl_carries_the_copied_math` pins them. The density and
// lighting models are the game's own (C1/C2).
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
    /// xyz: illuminance of the sun outside the atmosphere per channel, lx. w: ambient source
    /// term E_d / 2π, cd/m² (C2).
    light_e: vec4<f32>,
    /// xy: cloud drift for the weather field (wrapped to its tile period), m (C1). zw: for the
    /// base noise (wrapped to its tile period), m (C1).
    drift: vec4<f32>,
    /// xy: cloud drift for the detail noise (wrapped to its tile period), m (C1). zw: unit wind
    /// direction (XZ) (C1).
    drift_detail: vec4<f32>,
    /// xy: world XZ of the weather map's minimum corner, m (C1). z: its extent, m (C1). w: unused.
    weather: vec4<f32>,
    /// x: frame counter. y: primary march steps Q. z: light samples. w: unused.
    counts: vec4<u32>,
    /// xy: full resolution. zw: march resolution.
    sizes: vec4<u32>,
    /// x: debug mode (0 off, 1 uniform shell of luminance y). y: debug shell luminance.
    /// z: unused. w: 1 on the first frame(s) of a history (no valid history).
    misc: vec4<f32>,
    /// x: world units per metre (`ElementsUnits`). Everything above is in metres; Bevy's view
    /// matrices and depth are in world units, so the shaders divide what they take from them by
    /// this and multiply what they hand back.
    units: vec4<f32>,
}

const PI: f32 = 3.14159265358979;
/// Farthest distance the march goes, m. The camera looks up 22.55–60° into a
/// shell at 0.8–3.2 km, so visible cloud is within ~10 km; 16 km leaves slack
/// for the far veil (C2) without sampling the full aerial LUT range.
const MAX_MARCH_M: f32 = 16000.0;
/// Large number standing for "no surface": the sky.
const NO_SURFACE: f32 = 1.0e30;

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

/// Henyey–Greenstein phase function, sr⁻¹ (used by the lighting model, C2).
fn hg(cos_t: f32, g: f32) -> f32 {
    let g2 = g * g;
    return (1.0 - g2) / (4.0 * PI * pow(1.0 + g2 - 2.0 * g * cos_t, 1.5));
}

/// The single-scattering dual lobe: forward lobe (g = 0.8) weighted 0.7, backward lobe
/// (g = −0.5) weighted 0.3. The backward lobe is what turns a backlit cumulus rim silver.
fn phase(cos_t: f32) -> f32 {
    return 0.7 * hg(cos_t, 0.8) + 0.3 * hg(cos_t, -0.5);
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
/// illuminance through the LUT comes out amber (C2).
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
