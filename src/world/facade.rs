//! Procedural facade textures (R3.11): the wall itself, its windows, its glass
//! and the lights behind them.
//!
//! # Why textures instead of geometry
//!
//! A window is a hole 1 m across in a wall 20 m up. Modelling frames, reveals
//! and sills as geometry would cost more triangles than the whole city's walls,
//! and at the walking distance of this game the eye reads *pattern*, not
//! profile. So the facade is one tiling texture per style, and the box stays a
//! box.
//!
//! Three maps travel together, all sampled with the same UVs:
//!
//! | Map | Carries | Why |
//! |---|---|---|
//! | `base_color` (sRGB) | plaster / brick / concrete / corrugated steel, glass, frames, sills, rain streaks | the silhouette and the colour |
//! | `metallic_roughness` (linear) | glass = smooth dielectric, wall = rough | **this is the glass reflection**: a smooth dielectric mirrors the sky's IBL at grazing angles, which is the whole point of the rewrite |
//! | `emissive` (sRGB mask) | a few windows lit from inside, warm or fluorescent | dusk needs a handful of lit windows or the city reads as abandoned |
//!
//! `base_color` stays near-neutral and the style tint continues to come from
//! the material (`placement::StylePalette`), so the R3.6 wear buckets keep
//! working unchanged: wear multiplies the tint, never the pattern.
//!
//! # Tileability is structural, not eyeballed
//!
//! [`sample`] is the single source of truth, and every term in it is periodic
//! over one tile: cell indices wrap modulo the bay/floor count, brick rows wrap,
//! and the noise is *periodic* value noise whose lattice wraps. The baker just
//! loops over texel centres and calls it. `sampling_wraps_at_the_tile_seam`
//! asserts `sample(u + 1, v) == sample(u, v)` exactly, so a future term that
//! forgets to wrap fails a test instead of showing a seam on every wall.
//!
//! # What is deliberately NOT here
//!
//! - **No normal map.** UVs are local-space (see `assets/shaders/facade_uv.wesl`)
//!   and the mesh has no tangents; a normal map would need mikktspace tangents
//!   on a unit cube scaled non-uniformly, which does not exist.
//! - **No plinth / shopfront / roof band.** Anything that belongs to *one
//!   building* cannot live in a texture that repeats every 12 m. Those are
//!   geometry (R3.11 step 11.3/11.4).
//! - **No lit/unlit variants.** The lit pattern repeats every tile; two
//!   variants per style (picked per building) are what breaks the rhythm.

use bevy::asset::RenderAssetUsages;
use bevy::image::{Image, ImageSampler};
use bevy::math::Vec2;
use bevy::render::render_resource::{Extent3d, TextureDataOrder, TextureDimension, TextureFormat};

use crate::world::layout_graph::BlockType;

/// Bays (window columns) per texture tile.
pub const BAYS_PER_TILE: u32 = 4;
/// Floors per texture tile.
pub const FLOORS_PER_TILE: u32 = 4;
/// Distinct patterns per style, picked per building to break the tile rhythm.
pub const VARIANT_COUNT: u8 = 2;
/// Base-colour resolution. 1024 over a ~13 m tile is ~78 px/m: soft at arm's
/// length, indistinguishable at dusk, and it keeps the whole set (3 styles x 2
/// variants) at ~34 MB.
pub const BASE_SIZE: u32 = 1024;
/// Roughness/metallic and emissive resolution. Both are broad masks (a pane, a
/// joint), so half the base resolution loses nothing visible.
pub const CHANNEL_SIZE: u32 = 512;

/// Deeper than this and a facade texel is bigger than the thing it describes.
const MIN_MIP_SIZE: u32 = 4;

/// Style of a facade. One row per building type in `placement`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FacadeStyle {
    /// 老城: plaster and grey brick, small casement windows, 1-2 storeys of 3 m.
    OldCity,
    /// 旧公寓: pale concrete panels, wide 3-pane windows, 3 m floors.
    Apartment,
    /// 旧厂房: corrugated steel, ribbon windows, 6 m floors.
    Factory,
    /// 城中村: self-built plaster houses, small windows, 3 m floors. Shares
    /// the old city's wall language (brick-and-plaster, small casements) but
    /// bakes its own variant: the style seeds the pattern, so village walls
    /// never repeat an old-city tile.
    UrbanVillage,
}

/// Number of styles in [`FacadeStyle::ALL`]. Typed arrays in `chunks.rs` and
/// the baking loops are sized from this.
pub const STYLE_COUNT: usize = 4;

impl FacadeStyle {
    pub const ALL: [FacadeStyle; STYLE_COUNT] = [
        FacadeStyle::OldCity,
        FacadeStyle::Apartment,
        FacadeStyle::Factory,
        FacadeStyle::UrbanVillage,
    ];

    /// The facade style of a block type, or `None` for walkable blocks.
    pub fn of(kind: BlockType) -> Option<Self> {
        match kind {
            BlockType::OldCity => Some(FacadeStyle::OldCity),
            BlockType::Apartment => Some(FacadeStyle::Apartment),
            BlockType::Factory => Some(FacadeStyle::Factory),
            BlockType::UrbanVillage => Some(FacadeStyle::UrbanVillage),
            BlockType::Plaza | BlockType::Forest => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FacadeStyle::OldCity => "old_city",
            FacadeStyle::Apartment => "apartment",
            FacadeStyle::Factory => "factory",
            FacadeStyle::UrbanVillage => "urban_village",
        }
    }

    /// Bay pitch and floor height, in metres.
    ///
    /// These are *the* numbers the shader divides by, so they decide both the
    /// window rhythm and the texel density. The bay also drives
    /// [`snap_to_bay`]: a facade that ends mid-bay cuts a window in half at the
    /// corner, which is the one artifact this approach cannot hide.
    pub fn dims(self) -> FacadeDims {
        match self {
            // 2.0 m bays: a 0.9 m casement with a 1.1 m pier, the rhythm of a
            // shophouse row. 4-6 m modules snap to 4.0 or 6.0 m (2 or 3 bays).
            FacadeStyle::OldCity => FacadeDims {
                bay: 2.0,
                floor: 3.0,
            },
            // 3.3 m bays: a 2.2 m three-pane window, the standard 1980s slab.
            FacadeStyle::Apartment => FacadeDims {
                bay: 3.3,
                floor: 3.0,
            },
            // 6 m bays: one structural bay of a sawtooth shed.
            FacadeStyle::Factory => FacadeDims {
                bay: 6.0,
                floor: 6.0,
            },
            // Same rhythm as the old city: self-built houses sit on the same
            // 2.0 m bay / 3.0 m floor grid, only their wall pattern differs.
            FacadeStyle::UrbanVillage => FacadeDims {
                bay: 2.0,
                floor: 3.0,
            },
        }
    }

    /// Per-style window geometry, in bay/floor fractions.
    fn window(self) -> WindowSpec {
        match self {
            FacadeStyle::OldCity => WindowSpec {
                width: 0.45,
                top: 0.25,
                bottom: 0.70,
                panes: 2,
                blank: 0.22,
                lit: 0.42,
            },
            // Village houses: smaller openings, more blank piers (walls that
            // were extended floor by floor leave irregular fenestration), and
            // slightly more lit rooms (denser living).
            FacadeStyle::UrbanVillage => WindowSpec {
                width: 0.42,
                top: 0.28,
                bottom: 0.74,
                panes: 2,
                blank: 0.30,
                lit: 0.46,
            },
            FacadeStyle::Apartment => WindowSpec {
                width: 0.66,
                top: 0.25,
                bottom: 0.75,
                panes: 3,
                blank: 0.10,
                lit: 0.34,
            },
            FacadeStyle::Factory => WindowSpec {
                width: 0.88,
                top: 0.30,
                bottom: 0.735,
                panes: 9,
                blank: 0.04,
                lit: 0.16,
            },
        }
    }
}

/// The metric size of one bay and one floor (m).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FacadeDims {
    pub bay: f32,
    pub floor: f32,
}

impl FacadeDims {
    /// World size of one texture tile, in metres (u, v).
    pub fn tile(self) -> Vec2 {
        Vec2::new(
            self.bay * BAYS_PER_TILE as f32,
            self.floor * FLOORS_PER_TILE as f32,
        )
    }
}

/// Rounds a length to a whole number of bays (at least one).
///
/// The shader derives `u` from the building's own local extent, so a facade
/// that is not a whole number of bays ends with a sliced window at the corner.
/// Snapping the *footprint* instead of stretching the bays keeps every window
/// the same physical size, which is what makes the street read as built rather
/// than tiled.
pub fn snap_to_bay(value: f32, bay: f32) -> f32 {
    (value / bay).round().max(1.0) * bay
}

/// Window geometry of one style, as fractions of the bay (x) and floor (y).
#[derive(Clone, Copy, Debug)]
struct WindowSpec {
    /// Window width / bay width.
    width: f32,
    /// Window top / bottom, from the floor band's ceiling (the tile's v = 0).
    top: f32,
    bottom: f32,
    /// Vertical panes; `panes - 1` mullions.
    panes: u32,
    /// Fraction of bays with no window (party walls, blank gables).
    blank: f32,
    /// Fraction of panes lit from inside.
    lit: f32,
}

/// One texel's worth of PBR inputs, in the units each map stores.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    /// sRGB 0..1.
    pub albedo: [f32; 3],
    /// 0 = mirror, 1 = fully diffuse.
    pub roughness: f32,
    /// 0 = dielectric, 1 = metal.
    pub metallic: f32,
    /// sRGB 0..1, multiplied by the material's emissive luminance.
    pub emissive: [f32; 3],
}

/// The facade as a continuous function of tile space `(u, v)`, both in `[0, 1)`.
///
/// `v = 0` is the roofline: the shader aligns `v` to the top of the wall (see
/// the shader header for why the bottom is the safe place to lose a fraction of
/// a floor).
pub fn sample(style: FacadeStyle, variant: u8, u: f32, v: f32) -> Surface {
    let spec = style.window();
    let bays = BAYS_PER_TILE as f32;
    let floors = FLOORS_PER_TILE as f32;

    // Cell coordinates. Indices wrap so the pattern is periodic by construction.
    let fu = u * bays;
    let fv = v * floors;
    let bay = (fu.floor() as i32).rem_euclid(BAYS_PER_TILE as i32) as u32;
    let floor = (fv.floor() as i32).rem_euclid(FLOORS_PER_TILE as i32) as u32;
    let fx = fu - fu.floor();
    let fy = fv - fv.floor();

    let seed = variant as u32 * 7919 + style as u32 * 104_729;
    let mut surface = match style {
        FacadeStyle::OldCity => old_city_wall(style, seed, bay, floor, fx, fy, u, v),
        // The village shares the old city's brick-and-plaster wall body; the
        // style reaches the function through `style.dims()` and the seed, so
        // village walls bake their own tiles without a second body.
        FacadeStyle::UrbanVillage => old_city_wall(style, seed, bay, floor, fx, fy, u, v),
        FacadeStyle::Apartment => apartment_wall(seed, bay, floor, fx, fy, u, v),
        FacadeStyle::Factory => factory_wall(seed, bay, floor, fx, fy, u, v),
    };

    // Window opening: centred, with a per-bay horizontal wander so a street does
    // not look punched by a machine.
    let wander = (hash01(bay, floor, seed ^ 0x51ed) - 0.5) * 0.06;
    let half = spec.width * 0.5;
    let in_window = fx >= 0.5 + wander - half
        && fx <= 0.5 + wander + half
        && fy >= spec.top
        && fy <= spec.bottom
        && hash01(bay, floor, seed ^ 0x9e37) >= spec.blank;

    let x0 = 0.5 + wander - half;
    let x1 = 0.5 + wander + half;
    let px = (fx - x0) / (x1 - x0);
    let py = (fy - spec.top) / (spec.bottom - spec.top);
    let opening = in_window_partial(fx, fy, &spec, wander);

    if in_window {
        let pane = pane_at(px, py, spec.panes, bay, floor, spec.lit, seed);
        surface = window_surface(style, &surface, &pane, px, py, seed);
    } else if !opening {
        // Nothing: keep the wall.
    } else {
        // Frame / reveal band: slightly darker (occlusion), matte.
        surface.albedo = mix3(surface.albedo, [0.30, 0.29, 0.27], 0.45);
        surface.roughness = surface.roughness.max(0.8);
        // ...but it is not pitch black next to a lit room. The reveal and the
        // sill catch the spill, strongest just under the opening and fading
        // down the sill, weakest above the lintel. This is the cue that ties
        // the glowing pane to the wall around it instead of letting it float.
        let pane = pane_at(
            px.clamp(0.02, 0.98),
            py.clamp(0.02, 0.98),
            spec.panes,
            bay,
            floor,
            spec.lit,
            seed,
        );
        if pane.lit {
            let (colour, glow) = pane_light(style, &pane);
            let vertical = if fy > spec.bottom {
                (1.0 - ((fy - spec.bottom) / 0.055).min(1.0)) * 0.16
            } else if fy < spec.top {
                0.035
            } else {
                0.09
            };
            surface.emissive = scale3(colour, glow * vertical);
        }
    }

    // Sill: a bright lip under every window, catching sky light. This single
    // line does more for "there is a window here" than any other detail.
    if in_window_partial(fx, fy, &spec, wander) && fy > spec.bottom && fy < spec.bottom + 0.055 {
        surface.albedo = mix3(surface.albedo, [0.72, 0.70, 0.65], 0.55);
    }
    // Lintel shadow just above the opening.
    if in_window_partial(fx, fy, &spec, wander) && fy < spec.top && fy > spec.top - 0.05 {
        surface.albedo = scale3(surface.albedo, 0.72);
    }

    surface
}

/// True when `(fx, fy)` is inside the opening's bounding box, lit or not.
fn in_window_partial(fx: f32, fy: f32, spec: &WindowSpec, wander: f32) -> bool {
    let half = spec.width * 0.5;
    fx >= 0.5 + wander - half - 0.02
        && fx <= 0.5 + wander + half + 0.02
        && fy >= spec.top - 0.02
        && fy <= spec.bottom + 0.02
}

/// Which pane of the window a point falls in, and whether it is lit.
struct Pane {
    /// True when the point is inside the glass, not on a mullion or frame.
    glass: bool,
    /// True when the room behind the pane is lit. Deliberately independent of
    /// `glass`: a mullion is a solid bar in a lit room, and its sides catch the
    /// light (see `window_surface`), so "lit" must survive the glass test.
    lit: bool,
    /// 0..1 brightness of the light behind it.
    glow: f32,
    /// Deterministic per-pane seed, so the light's colour and reach are stable
    /// across bakes and every building using this variant agrees.
    seed: u32,
    /// Where the lamp sits inside the pane, 0..1 from the pane's left/top.
    ///
    /// Off-centre on purpose: a pane whose brightest point is its exact centre
    /// reads as a decal. A ceiling lamp above eye level plus a floor lamp or a
    /// television to one side puts the hot spot off-axis, which is the single
    /// cheapest depth cue a texture can carry.
    source: (f32, f32),
    /// Fraction of the pane covered by a curtain, 0 = none.
    curtain: f32,
    /// Whether that curtain hangs from the right edge.
    curtain_right: bool,
}

fn pane_at(px: f32, py: f32, panes: u32, bay: u32, floor: u32, lit_chance: f32, seed: u32) -> Pane {
    let panes_f = panes as f32;
    let cell = (px * panes_f).floor().clamp(0.0, panes_f - 1.0);
    let pane_index = cell as u32;
    let local = px * panes_f - cell;
    let pane_seed = hash_u32(bay ^ hash_u32(floor ^ hash_u32(pane_index ^ hash_u32(seed))));
    // 8% frame margin on every pane edge: the glow must not touch the frame.
    let margin = 0.08;
    let glass = local > margin && local < 1.0 - margin && py > 0.05 && py < 0.95;
    // Lit is a property of the room, not of the bay or the floor: hashing the
    // pane index with the building variant is enough, and it keeps a whole
    // floor from switching on at once (which reads as an office, not a home).
    let lit = hash01(pane_index, pane_seed & 7, seed ^ 0xa5) < lit_chance;
    // The lamp hangs in the upper half (ceiling / wall lamp) and off to one
    // side by up to a third of the pane. Both are per-room, so neighbouring
    // panes never share a hot spot.
    let source = (
        0.30 + 0.40 * hash01(pane_seed, 11, seed),
        0.24 + 0.18 * hash01(pane_seed, 13, seed),
    );
    // One pane in five has a curtain drawn part-way. It is only visible in a
    // lit room (an unlit pane is already black), and it is what breaks the
    // "glowing rectangle" silhouette into a room with something in it.
    let curtain_roll = hash01(pane_seed, 17, seed);
    let curtain = if curtain_roll < 0.20 {
        0.28 + 0.34 * hash01(pane_seed, 19, seed)
    } else {
        0.0
    };
    Pane {
        glass,
        lit,
        // Reach, so neighbouring lit panes are not equally bright.
        glow: 0.45 + 0.55 * hash01(pane_seed, 7, seed),
        seed: pane_seed,
        source,
        curtain,
        curtain_right: hash01(pane_seed, 23, seed) > 0.5,
    }
}

/// Colour temperature and brightness of the light in one room.
///
/// Split from [`window_surface`] because the frame, the sill and the glass all
/// need the same answer: a lit pane whose mullion is lit by a different colour
/// reads as two objects, not one window.
fn pane_light(style: FacadeStyle, pane: &Pane) -> ([f32; 3], f32) {
    // Per-pane light: colour temperature by style, plus one odd window in ten
    // (a television, a fluorescent tube, a bare bulb).
    match style {
        FacadeStyle::OldCity => {
            if pane.seed.is_multiple_of(11) {
                ([1.0, 0.55, 0.34], 0.62)
            } else {
                ([1.0, 0.74, 0.44], pane.glow)
            }
        }
        FacadeStyle::Apartment => {
            if pane.seed.is_multiple_of(7) {
                ([0.58, 0.70, 1.0], 0.55)
            } else if pane.seed.is_multiple_of(5) {
                ([1.0, 0.62, 0.40], 0.50)
            } else {
                ([1.0, 0.80, 0.52], pane.glow)
            }
        }
        FacadeStyle::Factory => {
            if pane.seed.is_multiple_of(4) {
                ([1.0, 0.72, 0.46], 0.45)
            } else {
                ([0.74, 0.86, 1.0], 0.55 + 0.3 * pane.glow)
            }
        }
        // Village rooms: warmer than the old city, one bare fluorescent tube
        // in twelve (workrooms, corner shops).
        FacadeStyle::UrbanVillage => {
            if pane.seed.is_multiple_of(12) {
                ([0.72, 0.85, 1.0], 0.48)
            } else {
                ([1.0, 0.70, 0.40], pane.glow)
            }
        }
    }
}

/// Luminous power of one lit room's spill light (lumens).
///
/// First-tune parameter. The light sits outside the facade, so it stands in
/// for the *whole* room's spill rather than one bulb. Too high and the wall
/// around the window blows out; too low and nothing reaches the pavement. The
/// C step exists to be judged on screen, so this number is deliberately a
/// single constant.
pub const WINDOW_GLOW_LUMENS: f32 = 1400.0;

/// Cut-off radius of a window spill light (m).
pub const WINDOW_GLOW_RANGE: f32 = 7.0;

/// Distance from the facade plane to the spill light (m).
///
/// Outside, not inside: without shadows a light behind the wall cannot light
/// the wall's outward normal at all. A metre out also keeps the hot spot off
/// the plaster while still catching the sill and the pavement.
pub const WINDOW_GLOW_OFFSET: f32 = 1.1;

/// One lit window on a building facade, in tile terms.
///
/// C only needs a position and a colour, but keeping the bay/floor indices
/// makes the value testable without a renderer: a light that lands outside the
/// facade is a silently invisible lamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowLight {
    /// Bay along the frontage, 0-based.
    pub bay: u32,
    /// Floor from the roofline, 0 = top.
    pub floor: u32,
    /// 0..1 across the bay (the lamp's horizontal position, not the pane's).
    pub fx: f32,
    /// 0..1 down the floor (0 = ceiling).
    pub fy: f32,
    /// sRGB light colour.
    pub tint: [f32; 3],
    /// 0..1 brightness of the room.
    pub glow: f32,
}

/// The lowest open lit window on a facade, if it has one.
///
/// Lowest, not brightest: a point light behind a ninth-floor pane lights
/// nothing a walker can see. `bay_count` and `floor_count` are the whole-bay /
/// whole-floor multiples placement snapped the building to; the texture
/// repeats every four of each, so the pattern is evaluated modulo the tile.
pub fn street_window(
    style: FacadeStyle,
    variant: u8,
    bay_count: u32,
    floor_count: u32,
) -> Option<WindowLight> {
    let spec = style.window();
    let seed = variant as u32 * 7919 + style as u32 * 104_729;
    let mut best: Option<WindowLight> = None;
    for floor in (0..floor_count).rev() {
        let cell_floor = floor % FLOORS_PER_TILE;
        for bay in 0..bay_count {
            let cell_bay = bay % BAYS_PER_TILE;
            if hash01(cell_bay, cell_floor, seed ^ 0x9e37) < spec.blank {
                continue;
            }
            let wander = (hash01(cell_bay, cell_floor, seed ^ 0x51ed) - 0.5) * 0.06;
            for pane_index in 0..spec.panes {
                let center = (pane_index as f32 + 0.5) / spec.panes as f32;
                let pane = pane_at(
                    center, 0.5, spec.panes, cell_bay, cell_floor, spec.lit, seed,
                );
                // A drawn curtain hides the lamp: a point light there would
                // light the street from behind fabric, which is worse than no
                // light at all.
                if !pane.lit || pane.curtain > 0.0 {
                    continue;
                }
                let (tint, glow) = pane_light(style, &pane);
                let window_pos = center + (pane.source.0 - 0.5) / spec.panes as f32;
                let candidate = WindowLight {
                    bay,
                    floor,
                    fx: 0.5 + wander + (window_pos - 0.5) * spec.width,
                    fy: spec.top + pane.source.1 * (spec.bottom - spec.top),
                    tint,
                    glow,
                };
                if best.is_none_or(|b| candidate.glow > b.glow) {
                    best = Some(candidate);
                }
            }
        }
        // The lowest floor wins even if a higher room is brighter.
        if best.is_some() {
            break;
        }
    }
    best
}

/// Turns the wall into glass, frame or reflection.
fn window_surface(
    style: FacadeStyle,
    wall: &Surface,
    pane: &Pane,
    px: f32,
    py: f32,
    seed: u32,
) -> Surface {
    let (colour, glow) = pane_light(style, pane);

    if !pane.glass {
        // Mullion / frame bar: painted metal, matte, darker than the wall so it
        // reads as a line at silhouette distance.
        let frame = match style {
            FacadeStyle::OldCity => [0.50, 0.46, 0.40],
            FacadeStyle::Apartment => [0.52, 0.53, 0.54],
            FacadeStyle::Factory => [0.34, 0.33, 0.31],
            // Village frames: cheap painted steel, a little bluer.
            FacadeStyle::UrbanVillage => [0.46, 0.45, 0.43],
        };
        let mut surface = Surface {
            albedo: mix3(wall.albedo, frame, 0.75),
            roughness: 0.62,
            metallic: 0.0,
            emissive: [0.0; 3],
        };
        if pane.lit {
            // A lit room wraps its light around the mullion instead of
            // stopping at the glass edge. Without this the frame is a black
            // bar pasted over a glowing rectangle, which is the "sticker"
            // look the whole rewrite is trying to avoid.
            let panes = style.window().panes as f32;
            let cell = (px * panes).floor().clamp(0.0, panes - 1.0);
            let local = px * panes - cell;
            let edge = local.min(1.0 - local).min(py.min(1.0 - py) * 3.0);
            let wrap = 1.0 - (edge / 0.08).clamp(0.0, 1.0);
            surface.emissive = scale3(colour, 0.09 * glow * wrap);
        }
        return surface;
    }

    // Glass: dark, because a pane at dusk shows the room behind it, not the
    // wall. The sky's reflection arrives through the *specular* term, so the
    // albedo stays near-black and the roughness does the work.
    let top_brightness = 0.020 * (1.0 - py);
    let tint = match style {
        FacadeStyle::OldCity => [0.055, 0.062, 0.070],
        FacadeStyle::Apartment => [0.048, 0.056, 0.066],
        FacadeStyle::Factory => [0.070, 0.074, 0.072],
        FacadeStyle::UrbanVillage => [0.052, 0.058, 0.062],
    };
    // A soft diagonal sheen in the glass itself. The physical reflection is
    // only ~4% at these angles and the sky it mirrors is a smooth gradient, so
    // the pane needs a little structure of its own to read as a *surface*
    // rather than a hole. Kept in the albedo, not emissive: it must still be
    // lit, or it becomes a sticker in a different colour.
    let band = (px * 0.72 + (1.0 - py) * 0.28 - 0.38).abs();
    let sheen = 0.045 * (1.0 - (band / 0.38).min(1.0));
    // Corrugation / dust: a little variation so panes are not identical.
    let dust = 0.004 * hash01(pane.seed, 3, seed);
    let albedo = [
        tint[0] + top_brightness + dust + sheen,
        tint[1] + top_brightness + dust + sheen,
        tint[2] + top_brightness * 1.15 + dust + sheen * 1.1,
    ];

    // Factory glass is dirtier and its frames thinner; old-city glass is the
    // cleanest (small panes, replaced often).
    let roughness = match style {
        FacadeStyle::OldCity => 0.055 + 0.05 * hash01(pane.seed, 5, seed),
        FacadeStyle::Apartment => 0.045 + 0.05 * hash01(pane.seed, 5, seed),
        FacadeStyle::Factory => 0.10 + 0.16 * hash01(pane.seed, 5, seed),
        // Village glass: cheap single glazing, so a touch cloudier.
        FacadeStyle::UrbanVillage => 0.07 + 0.06 * hash01(pane.seed, 5, seed),
    };

    // A lamp inside the room: a hot core at the lamp itself, a broad room
    // fill around it, and the reveal eating the light at the pane's edge.
    // This is what stops a lit window from reading as a flat sticker: the eye
    // reads the hot spot, the falloff and the occluded border as a *room*.
    let (sx, sy) = pane.source;
    let dx = (px - sx) * 1.45;
    let dy = py - sy;
    let d2 = dx * dx + dy * dy;
    let core = (-14.0 * d2).exp();
    let room = (1.0 - (d2 * 1.15).min(1.0)).max(0.0);
    let fill = 0.55 * room * room;

    // The reveal: the top of the pane sits under a lintel and the sides under
    // a frame, so the light dims before it reaches the edge.
    let edge_x = (px.min(1.0 - px) * 2.0).min(1.0);
    let edge_y = (py.min(1.0 - py) * 3.0).min(1.0);
    let reveal = 0.42 + 0.58 * smoothstep01(edge_x * edge_y);
    let lintel = 0.55 + 0.45 * smoothstep01((py / 0.14).min(1.0));

    // A drawn curtain on one side: a soft edge with a sliver of light leaking
    // past it. Only one pane in five, so it reads as a lived-in room rather
    // than a pattern.
    let curtain = if pane.curtain > 0.0 {
        let edge = if pane.curtain_right {
            1.0 - pane.curtain
        } else {
            pane.curtain
        };
        let covered = if pane.curtain_right {
            px > edge
        } else {
            px < edge
        };
        let soft = smoothstep01(((px - edge).abs() / 0.03).min(1.0));
        if covered {
            0.35 + 0.65 * (1.0 - soft)
        } else {
            0.35 + 0.65 * soft
        }
    } else {
        1.0
    };

    let level = if pane.lit {
        ((core + fill) * reveal * lintel * curtain).min(1.0) * 0.95
    } else {
        0.0
    };
    let emissive = scale3(colour, glow * level);

    Surface {
        albedo,
        roughness,
        metallic: 0.0,
        emissive,
    }
}

// --- per-style wall bodies -------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn old_city_wall(
    style: FacadeStyle,
    seed: u32,
    bay: u32,
    floor: u32,
    fx: f32,
    fy: f32,
    u: f32,
    v: f32,
) -> Surface {
    let dims = style.dims();
    let tile = dims.tile();
    // Brick counts derived from the tile, not hard-coded: a course that does not
    // divide the tile exactly shifts one row at the seam, and the hash below
    // would have no period to wrap.
    let courses = (tile.y / 0.08).round() as u32;
    let per_course = (tile.x / 0.25).round() as u32;
    // 70% pale plaster, 30% grey brick.
    let brick = hash01(bay, 0, seed ^ 0x2b1d) < 0.30;
    let mut albedo = if brick {
        let row = (v * courses as f32).floor() as i32;
        let col = ((u * per_course as f32) + 0.5 * (row.rem_euclid(2)) as f32)
            .rem_euclid(per_course as f32) as u32;
        let jitter = 0.85 + 0.3 * hash01(col, row.rem_euclid(courses as i32) as u32, seed);
        [0.52 * jitter, 0.42 * jitter, 0.37 * jitter]
    } else {
        let mottle = fbm(u, v, 24, seed, 3);
        let tone = 0.62 + 0.10 * mottle;
        [tone * 1.06, tone, tone * 0.90]
    };

    // Rain: vertical streaks. Two octaves of noise that is narrow in `u` and
    // tall in `v` (freq_u >> freq_v) is enough to read as "this wall has been wet
    // for thirty years". The anisotropy lives in the lattice frequencies, NOT in
    // a `v * 0.06` multiplier: scaling the coordinate inside `pnoise2` would
    // stop the lattice from wrapping and put a seam on every wall.
    let streak = 0.5 * pnoise2(u, v, 96, 6, seed + 17) + 0.5 * pnoise2(u, v, 192, 3, seed + 23);
    if streak > 0.58 {
        let amount = ((streak - 0.58) / 0.42).min(1.0) * 0.28;
        albedo = scale3(albedo, 1.0 - amount);
    }

    // A floor band: the slab edge, 0.18 m of slightly darker render at each
    // floor line. Real shophouses show this and it makes floors countable.
    //
    // There is deliberately no pavement-damp gradient here: a repeating tile
    // cannot know which of its copies is the ground floor, and pretending
    // otherwise would darken the same band four storeys up.
    if fy > 0.965 {
        albedo = scale3(albedo, 0.80);
    }

    let _ = fx;
    Surface {
        albedo,
        roughness: 0.82 + 0.10 * hash01(bay, floor, seed ^ 0x77),
        metallic: 0.0,
        emissive: [0.0; 3],
    }
}

fn apartment_wall(seed: u32, bay: u32, floor: u32, fx: f32, fy: f32, u: f32, v: f32) -> Surface {
    // Precast concrete panels: joint lines on the bay and floor boundaries are
    // what makes a slab block read as a slab block.
    let mottle = fbm(u, v, 16, seed + 3, 4);
    let tone = 0.60 + 0.07 * mottle;
    let mut albedo = [tone * 0.99, tone, tone * 1.03];

    let joint = fx < 0.018 || fx > 0.982 || fy > 0.982;
    if joint {
        albedo = mix3(albedo, [0.40, 0.40, 0.39], 0.55);
    }
    // Rust bleed from the joint anchors, a 1980s signature: streak-shaped, so
    // narrow in `u` and tall in `v` via the lattice frequencies.
    let rust = pnoise2(u, v, 48, 24, seed + 41);
    if rust > 0.72 && fx < 0.10 {
        albedo = mix3(albedo, [0.44, 0.27, 0.18], (rust - 0.72) * 1.6);
    }
    // Long vertical grime under every joint.
    let streak = pnoise2(u, v, 64, 5, seed + 59);
    if streak > 0.60 {
        albedo = scale3(albedo, 1.0 - (streak - 0.60) * 0.5);
    }

    let _ = (bay, floor);
    Surface {
        albedo,
        roughness: 0.88 + 0.08 * hash01(bay, floor, seed ^ 0x31),
        metallic: 0.0,
        emissive: [0.0; 3],
    }
}

fn factory_wall(seed: u32, bay: u32, _floor: u32, fx: f32, fy: f32, u: f32, v: f32) -> Surface {
    let _ = (fx, fy);
    // 40% of bays are brick infill, the rest corrugated steel.
    let tile = FacadeStyle::Factory.dims().tile();
    // Counts derived from the 24 x 24 m tile so the bond wraps at the seam.
    let courses = (tile.y / 0.075).round() as u32;
    let per_course = (tile.x / 0.23).round() as u32;
    let brick = hash01(bay, 1, seed ^ 0x6a09) < 0.40;
    let mut albedo = if brick {
        let row = (v * courses as f32).floor() as i32;
        let col = ((u * per_course as f32) + 0.5 * (row.rem_euclid(2)) as f32)
            .rem_euclid(per_course as f32) as u32;
        let jitter = 0.8 + 0.35 * hash01(col, row.rem_euclid(courses as i32) as u32, seed + 5);
        [0.40 * jitter, 0.30 * jitter, 0.26 * jitter]
    } else {
        // Corrugation: 120 ribs across the 24 m tile, so the rib pitch is
        // 0.2 m and the wave closes exactly at the seam.
        let rib = (u * std::f32::consts::TAU * 120.0).sin();
        let tone = 0.40 + 0.035 * rib;
        let rust = fbm(u, v, 12, seed + 11, 3);
        if rust > 0.60 {
            let t = ((rust - 0.60) / 0.40).min(1.0);
            mix3(
                [tone, tone * 0.98, tone * 0.94],
                [0.36, 0.22, 0.15],
                t * 0.8,
            )
        } else {
            [tone, tone * 0.99, tone * 0.95]
        }
    };

    // Concrete sill band under the ribbon window, and the steel frame line.
    let streak = pnoise2(u, v, 64, 8, seed + 71);
    if streak > 0.62 {
        albedo = scale3(albedo, 1.0 - (streak - 0.62) * 0.45);
    }
    if fy > 0.985 {
        albedo = scale3(albedo, 0.72);
    }

    Surface {
        albedo,
        // Steel walls are smoother than plaster but nowhere near glass.
        roughness: if brick { 0.90 } else { 0.62 },
        metallic: if brick { 0.0 } else { 0.35 },
        emissive: [0.0; 3],
    }
}

// --- baking ---------------------------------------------------------------

/// The three maps of one (style, variant) pair, as tightly packed RGBA8.
#[derive(Clone, Debug, PartialEq)]
pub struct FacadeTextures {
    pub base: Vec<u8>,
    pub base_size: u32,
    pub metallic_roughness: Vec<u8>,
    pub emissive: Vec<u8>,
    pub channel_size: u32,
}

/// Bakes one facade. Deterministic: the same style and variant always produce
/// the same bytes.
pub fn bake_facade(style: FacadeStyle, variant: u8) -> FacadeTextures {
    assert!(variant < VARIANT_COUNT, "variant out of range");
    FacadeTextures {
        base: bake_layer(style, variant, BASE_SIZE, |s| {
            [
                to_u8(s.albedo[0]),
                to_u8(s.albedo[1]),
                to_u8(s.albedo[2]),
                255,
            ]
        }),
        base_size: BASE_SIZE,
        // Bevy reads G = roughness, B = metallic for an RGBA map, and the
        // material's scalars multiply them (both stay 1.0, see `plugins::facade`).
        metallic_roughness: bake_layer(style, variant, CHANNEL_SIZE, |s| {
            [255, to_u8(s.roughness), to_u8(s.metallic), 255]
        }),
        emissive: bake_layer(style, variant, CHANNEL_SIZE, |s| {
            [
                to_u8(s.emissive[0]),
                to_u8(s.emissive[1]),
                to_u8(s.emissive[2]),
                255,
            ]
        }),
        channel_size: CHANNEL_SIZE,
    }
}

fn bake_layer(
    style: FacadeStyle,
    variant: u8,
    size: u32,
    encode: impl Fn(&Surface) -> [u8; 4],
) -> Vec<u8> {
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    let inv = 1.0 / size as f32;
    for y in 0..size {
        // Texel centres: never exactly 0 or 1, so the sample is always inside
        // the tile and the seam arithmetic never sees an endpoint.
        let v = (y as f32 + 0.5) * inv;
        for x in 0..size {
            let u = (x as f32 + 0.5) * inv;
            out.extend_from_slice(&encode(&sample(style, variant, u, v)));
        }
    }
    out
}

/// Uploads one map with a hand-built mip chain and repeat addressing.
///
/// Bevy does not generate mips for `Image` assets at runtime, and a facade
/// without them shimmers on every wall (the pattern is high-frequency and the
/// view is grazing). `Repeat` is required, not cosmetic: without it the last
/// texel column would be clamped along every bay boundary.
pub fn texture_image(rgba: &[u8], size: u32, srgb: bool) -> Image {
    let levels = mip_levels(size);
    let chain = mip_chain(rgba, size, levels);
    let mut data = Vec::with_capacity(chain.iter().map(|(_, d)| d.len()).sum());
    for (_, level) in &chain {
        data.extend_from_slice(level);
    }

    let format = if srgb {
        TextureFormat::Rgba8UnormSrgb
    } else {
        TextureFormat::Rgba8Unorm
    };
    // `Image::new` asserts the data length matches ONE level; a hand-built chain
    // has to go through `new_uninit` (the same trap `leaf_shape` documents).
    let mut image = Image::new_uninit(
        Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        format,
        RenderAssetUsages::default(),
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels;
    image.data_order = TextureDataOrder::MipMajor;
    image.sampler = ImageSampler::Descriptor(repeat_sampler());
    image
}

/// Repeat addressing, trilinear mips, 8x anisotropy.
///
/// The filters are not the defaults: `ImageSamplerDescriptor::default()` is
/// **nearest** for mag, min and mipmap, with `anisotropy_clamp: 1`. The mip
/// chain in [`upload_layer`] was added precisely because a high-frequency
/// facade shimmers on a grazing wall, and then sampled with a nearest mip --
/// which snaps between levels instead of blending them, and minifies without
/// filtering. Every window frame and every wall/plinth border crawled while the
/// camera walked. Linear + anisotropy keeps the wall calm at a distance and is
/// softer only where the texels are already magnified ~3x at arm's length
/// (1024 px across a ~10 m tile is 76 px/m; a 1080p frame at 2 m shows 240
/// px/m).
fn repeat_sampler() -> bevy::image::ImageSamplerDescriptor {
    bevy::image::ImageSamplerDescriptor {
        address_mode_u: bevy::image::ImageAddressMode::Repeat,
        address_mode_v: bevy::image::ImageAddressMode::Repeat,
        address_mode_w: bevy::image::ImageAddressMode::Repeat,
        mag_filter: bevy::image::ImageFilterMode::Linear,
        min_filter: bevy::image::ImageFilterMode::Linear,
        mipmap_filter: bevy::image::ImageFilterMode::Linear,
        anisotropy_clamp: 8,
        ..Default::default()
    }
}

/// Mip levels down to 4 px.
pub fn mip_levels(size: u32) -> u32 {
    let mut levels = 1;
    let mut current = size;
    while current > MIN_MIP_SIZE && levels < 12 {
        current /= 2;
        levels += 1;
    }
    levels
}

/// Box-filtered halving chain, `(size, data)` per level.
///
/// A box filter keeps the mean, and because the source is periodic the result
/// is periodic too -- so the seam stays closed at every level.
pub fn mip_chain(rgba: &[u8], size: u32, levels: u32) -> Vec<(u32, Vec<u8>)> {
    assert_eq!(rgba.len(), (size * size * 4) as usize, "rgba size mismatch");
    let mut chain = vec![(size, rgba.to_vec())];
    let mut current = size;
    while chain.len() < levels as usize && current > 1 {
        let next = (current / 2).max(1);
        let src = &chain.last().expect("non-empty").1;
        let mut dst = Vec::with_capacity((next * next * 4) as usize);
        for y in 0..next {
            for x in 0..next {
                let mut acc = [0u32; 4];
                for dy in 0..2 {
                    for dx in 0..2 {
                        let sx = (x * 2 + dx).min(current - 1);
                        let sy = (y * 2 + dy).min(current - 1);
                        let i = ((sy * current + sx) * 4) as usize;
                        for (c, slot) in acc.iter_mut().enumerate() {
                            *slot += src[i + c] as u32;
                        }
                    }
                }
                for slot in acc {
                    dst.push((slot / 4) as u8);
                }
            }
        }
        chain.push((next, dst));
        current = next;
    }
    chain
}

// --- helpers --------------------------------------------------------------

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn mix3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

fn scale3(a: [f32; 3], k: f32) -> [f32; 3] {
    [a[0] * k, a[1] * k, a[2] * k]
}

fn hash_u32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

fn hash01(a: u32, b: u32, c: u32) -> f32 {
    let h = hash_u32(a ^ hash_u32(b ^ hash_u32(c)));
    (h & 0x00ff_ffff) as f32 / 16_777_216.0
}

fn smoothstep01(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Value noise on a lattice that wraps every `freq` cells in both axes.
///
/// Wrapping the *lattice index* is what makes the noise periodic: the last cell
/// interpolates back to the first, so `u = 1` is `u = 0` by construction and no
/// seam can appear. Non-integer frequencies would break this, which is why
/// every caller passes a count, not a wavelength.
fn pnoise2(u: f32, v: f32, freq_u: u32, freq_v: u32, seed: u32) -> f32 {
    let x = u * freq_u as f32;
    let y = v * freq_v as f32;
    let ix = x.floor();
    let iy = y.floor();
    let fx = smoothstep01(x - ix);
    let fy = smoothstep01(y - iy);
    let i0 = (ix as i32).rem_euclid(freq_u as i32) as u32;
    let j0 = (iy as i32).rem_euclid(freq_v as i32) as u32;
    let i1 = (i0 + 1) % freq_u;
    let j1 = (j0 + 1) % freq_v;
    let a = hash01(i0, j0, seed);
    let b = hash01(i1, j0, seed);
    let c = hash01(i0, j1, seed);
    let d = hash01(i1, j1, seed);
    let top = a + (b - a) * fx;
    let bottom = c + (d - c) * fx;
    top + (bottom - top) * fy
}

/// Fractal sum of [`pnoise2`], same wrapped lattice at every octave.
fn fbm(u: f32, v: f32, freq: u32, seed: u32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 1.0;
    let mut total = 0.0;
    let mut f = freq;
    for o in 0..octaves {
        sum += pnoise2(u, v, f, f, seed + o * 977) * amp;
        total += amp;
        amp *= 0.5;
        f *= 2;
    }
    sum / total
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: [FacadeStyle; STYLE_COUNT] = FacadeStyle::ALL;

    /// Every style must end up as a wall with countable windows, not as noise.
    #[test]
    fn bays_and_floors_are_in_a_buildable_range() {
        for style in STYLES {
            let dims = style.dims();
            assert!(
                (1.6..=6.5).contains(&dims.bay),
                "{}: bay {} m is outside a believable bay pitch",
                style.label(),
                dims.bay
            );
            assert!(
                (2.4..=6.5).contains(&dims.floor),
                "{}: floor {} m is outside a storey height",
                style.label(),
                dims.floor
            );
        }
        // The tile must be a whole number of bays and floors by construction.
        for style in STYLES {
            let dims = style.dims();
            let tile = dims.tile();
            assert_eq!(tile.x / dims.bay, BAYS_PER_TILE as f32);
            assert_eq!(tile.y / dims.floor, FLOORS_PER_TILE as f32);
        }
    }

    /// The seam, at texel scale: the step from the last column of the tile to the
    /// first column of the next copy must look like any other neighbouring-column
    /// step. A pattern that merely *repeats* can still jump at the join (that is
    /// what a non-periodic noise octave looks like), and that jump is exactly the
    /// seam.
    #[test]
    fn the_tile_seam_is_no_harder_a_step_than_a_normal_texel() {
        let n = 128u32;
        let inv = 1.0 / n as f32;
        for style in STYLES {
            for variant in 0..VARIANT_COUNT {
                // A few rows, because some rows are pure wall and some cross a
                // window; both must join cleanly.
                for row in [3u32, 17, 40, 71, 100, 125] {
                    let v = (row as f32 + 0.5) * inv;
                    let mut normal_step = 0.0f32;
                    for x in 1..n {
                        let a = sample(style, variant, (x as f32 - 0.5) * inv, v);
                        let b = sample(style, variant, (x as f32 + 0.5) * inv, v);
                        normal_step = normal_step.max(distance(&a, &b));
                    }
                    let last = sample(style, variant, 1.0 - 0.5 * inv, v);
                    let first = sample(style, variant, 0.5 * inv, v);
                    let seam = distance(&last, &first);
                    assert!(
                        seam <= normal_step * 1.5 + 1e-6,
                        "{} variant {variant} row {row}: the seam jumps {seam:.4}, \
                         while the largest in-tile step is {normal_step:.4}",
                        style.label()
                    );
                }
            }
        }
    }

    /// Largest per-channel difference between two surfaces, in 0..1 units.
    fn distance(a: &Surface, b: &Surface) -> f32 {
        let mut worst: f32 = 0.0;
        for c in 0..3 {
            worst = worst.max((a.albedo[c] - b.albedo[c]).abs());
            worst = worst.max((a.emissive[c] - b.emissive[c]).abs());
        }
        worst.max((a.roughness - b.roughness).abs())
    }

    /// The pattern must actually be periodic: sampled a whole tile away it must
    /// agree, and that only holds if every term wraps its lattice.
    ///
    /// The comparison is a tolerance, not `==`: `u + 1.0` is not exactly
    /// representable in f32, so `fx` comes back a few ULPs off. That is
    /// arithmetic, not a pattern break -- the seam test above is what pins the
    /// visible property.
    #[test]
    fn sampling_is_periodic_outside_the_unit_square() {
        for style in STYLES {
            for variant in 0..VARIANT_COUNT {
                for (du, dv) in [(1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (-1.0, 0.0), (2.0, 3.0)] {
                    for i in 0..12 {
                        for j in 0..12 {
                            let u = (i as f32 + 0.37) / 12.0;
                            let v = (j as f32 + 0.61) / 12.0;
                            let base = sample(style, variant, u, v);
                            let shifted = sample(style, variant, u + du, v + dv);
                            assert!(
                                distance(&base, &shifted) < 1e-5,
                                "{} variant {variant}: shifted by ({du}, {dv}) at ({u}, {v}) \
                                 changed the surface by {:.3e}",
                                style.label(),
                                distance(&base, &shifted)
                            );
                        }
                    }
                }
            }
        }
    }

    /// A non-wrapping term is the classic way to put a seam on every tile, and
    /// it is invisible in a still frame. Pin the wrap on the noise helper
    /// itself: the lattice must return the same value at `u` and `u + 1`.
    #[test]
    fn the_noise_lattice_wraps() {
        for (fu, fv) in [(6u32, 3u32), (48, 24), (96, 6)] {
            for i in 0..10 {
                for j in 0..10 {
                    let u = (i as f32 + 0.31) / 10.0;
                    let v = (j as f32 + 0.73) / 10.0;
                    let a = pnoise2(u, v, fu, fv, 1234);
                    let b = pnoise2(u + 1.0, v + 1.0, fu, fv, 1234);
                    assert!(
                        (a - b).abs() < 1e-5,
                        "pnoise2({fu}, {fv}) does not wrap: {a} vs {b}"
                    );
                    assert!((0.0..=1.0).contains(&a), "noise outside 0..1: {a}");
                }
            }
        }
    }

    #[test]
    fn baking_is_deterministic() {
        for style in STYLES {
            let a = bake_facade(style, 0);
            let b = bake_facade(style, 0);
            assert_eq!(a, b, "{}: two bakes differ", style.label());
            assert_eq!(a.base.len(), (BASE_SIZE * BASE_SIZE * 4) as usize);
            assert_eq!(
                a.metallic_roughness.len(),
                (CHANNEL_SIZE * CHANNEL_SIZE * 4) as usize
            );
            assert_eq!(a.emissive.len(), a.metallic_roughness.len());
        }
    }

    #[test]
    fn variants_actually_differ() {
        for style in STYLES {
            let a = bake_facade(style, 0);
            let b = bake_facade(style, 1);
            let differing = a
                .base
                .chunks(4)
                .zip(b.base.chunks(4))
                .filter(|(x, y)| x != y)
                .count();
            let total = (BASE_SIZE * BASE_SIZE) as usize;
            assert!(
                differing * 100 > total,
                "{}: variants 0 and 1 differ on only {} of {total} texels",
                style.label(),
                differing
            );
        }
    }

    /// Statistics over one baked tile: the checks below all read these.
    fn stats(style: FacadeStyle, variant: u8) -> Stats {
        let mut stats = Stats::default();
        let n = CHANNEL_SIZE;
        let inv = 1.0 / n as f32;
        for y in 0..n {
            for x in 0..n {
                let s = sample(
                    style,
                    variant,
                    (x as f32 + 0.5) * inv,
                    (y as f32 + 0.5) * inv,
                );
                stats.total += 1;
                if s.roughness < 0.25 {
                    stats.glossy += 1;
                }
                if s.roughness > 0.6 {
                    stats.rough += 1;
                }
                if s.emissive[0] > 0.02 || s.emissive[1] > 0.02 || s.emissive[2] > 0.02 {
                    stats.lit += 1;
                }
                let luma = 0.2126 * s.albedo[0] + 0.7152 * s.albedo[1] + 0.0722 * s.albedo[2];
                stats.luma_sum += luma;
                stats.luma_max = stats.luma_max.max(luma);
                stats.emissive_r += s.emissive[0];
                stats.emissive_b += s.emissive[2];
            }
        }
        stats
    }

    #[derive(Default)]
    struct Stats {
        total: usize,
        glossy: usize,
        rough: usize,
        lit: usize,
        luma_sum: f32,
        luma_max: f32,
        emissive_r: f32,
        emissive_b: f32,
    }

    impl Stats {
        fn glossy_fraction(&self) -> f32 {
            self.glossy as f32 / self.total as f32
        }
        fn rough_fraction(&self) -> f32 {
            self.rough as f32 / self.total as f32
        }
        fn lit_fraction(&self) -> f32 {
            self.lit as f32 / self.total as f32
        }
        fn mean_luma(&self) -> f32 {
            self.luma_sum / self.total as f32
        }
    }

    /// Glass has to be a *minority* of the wall but not a rarity: too little and
    /// the street has no reflection, too much and it is a curtain wall.
    #[test]
    fn glass_is_smooth_and_the_wall_is_rough() {
        for style in STYLES {
            let stats = stats(style, 0);
            assert!(
                (0.03..0.45).contains(&stats.glossy_fraction()),
                "{}: {:.1}% of the facade is glass -- expected a window-sized minority",
                style.label(),
                stats.glossy_fraction() * 100.0
            );
            assert!(
                stats.rough_fraction() > 0.55,
                "{}: only {:.1}% of the facade is matte; the wall would read as glass",
                style.label(),
                stats.rough_fraction() * 100.0
            );
        }
    }

    /// Dusk needs a few lit windows: enough to say "people live here", few
    /// enough that the silhouette still dominates.
    #[test]
    fn a_few_windows_are_lit_and_most_are_not() {
        for style in STYLES {
            let stats = stats(style, 0);
            assert!(
                (0.004..0.20).contains(&stats.lit_fraction()),
                "{}: {:.2}% of the facade glows -- expected a handful of lit panes",
                style.label(),
                stats.lit_fraction() * 100.0
            );
        }
    }

    /// The albedo has to stay inside ADR-0001's silhouette budget: dark enough
    /// that the roofline cuts the sky, light enough to read as a wall.
    #[test]
    fn albedo_stays_inside_the_silhouette_budget() {
        for style in STYLES {
            let stats = stats(style, 0);
            let mean = stats.mean_luma();
            assert!(
                (0.12..0.60).contains(&mean),
                "{}: mean albedo luminance {mean:.3} is outside the dusk wall range",
                style.label()
            );
            assert!(
                stats.luma_max < 0.95,
                "{}: something is near-white ({:.3}); the wall has no silhouette left",
                style.label(),
                stats.luma_max
            );
        }
    }

    /// Warm interiors for housing, cold fluorescents for the sheds. If this
    /// flips, the city reads as a different decade.
    #[test]
    fn light_temperature_follows_the_building_type() {
        let old = stats(FacadeStyle::OldCity, 0);
        let factory = stats(FacadeStyle::Factory, 0);
        assert!(
            old.emissive_r > old.emissive_b * 1.6,
            "old-city windows must be tungsten-warm (r/b = {:.2})",
            old.emissive_r / old.emissive_b.max(1e-6)
        );
        assert!(
            factory.emissive_b > factory.emissive_r * 0.85,
            "factory windows must be cooler than a flat (r/b = {:.2})",
            factory.emissive_r / factory.emissive_b.max(1e-6)
        );
    }

    /// A tile holds four bays; if all four were identical the 12 m repeat would
    /// be obvious from a single glance along the street.
    #[test]
    fn the_four_bays_are_not_the_same_bay() {
        for style in STYLES {
            let mut first = None;
            let mut distinct = 0;
            for bay in 0..BAYS_PER_TILE {
                let u = (bay as f32 + 0.5) / BAYS_PER_TILE as f32;
                let signature: Vec<[u8; 4]> = (0..24)
                    .map(|j| {
                        let v = (j as f32 + 0.5) / 24.0;
                        let s = sample(style, 0, u, v);
                        [
                            to_u8(s.albedo[0]),
                            to_u8(s.roughness),
                            to_u8(s.emissive[0]),
                            to_u8(s.emissive[1]),
                        ]
                    })
                    .collect();
                match &first {
                    None => {
                        first = Some(signature);
                        distinct += 1;
                    }
                    Some(f) => {
                        if *f != signature {
                            distinct += 1;
                        }
                    }
                }
            }
            assert!(
                distinct >= 3,
                "{}: only {distinct} of {BAYS_PER_TILE} bays differ; the repeat will be visible",
                style.label()
            );
        }
    }

    #[test]
    fn snap_to_bay_rounds_to_whole_bays() {
        assert_eq!(snap_to_bay(4.9, 2.0), 4.0);
        assert_eq!(snap_to_bay(5.1, 2.0), 6.0);
        assert_eq!(snap_to_bay(25.0, 3.3), 26.4);
        // Never zero bays: a 0.4 m facade would otherwise vanish.
        assert_eq!(snap_to_bay(0.2, 3.3), 3.3);
        for raw in [1.0, 3.7, 12.2, 49.9] {
            let snapped = snap_to_bay(raw, 3.3);
            assert!((snapped / 3.3 - (snapped / 3.3).round()).abs() < 1e-4);
        }
    }

    #[test]
    fn mip_chain_reaches_four_texels_and_keeps_the_mean() {
        let dims = FacadeStyle::OldCity.dims();
        let _ = dims;
        let size = 64;
        let tex = FacadeTextures {
            base: bake_layer(FacadeStyle::OldCity, 0, size, |s| {
                [
                    to_u8(s.albedo[0]),
                    to_u8(s.albedo[1]),
                    to_u8(s.albedo[2]),
                    255,
                ]
            }),
            base_size: size,
            metallic_roughness: Vec::new(),
            emissive: Vec::new(),
            channel_size: 0,
        };
        let levels = mip_levels(size);
        let chain = mip_chain(&tex.base, size, levels);
        assert_eq!(chain.len() as u32, levels);
        assert_eq!(chain.last().expect("chain").0, 4);

        // A box filter preserves the mean; that is what keeps a wall from going
        // darker as it recedes.
        let mean = |data: &[u8]| {
            data.chunks(4)
                .map(|p| p[0] as f32 + p[1] as f32 + p[2] as f32)
                .sum::<f32>()
                / (data.len() / 4) as f32
        };
        let full = mean(&chain[0].1);
        let small = mean(&chain.last().expect("chain").1);
        assert!(
            (full - small).abs() < 6.0,
            "mip chain drifts in brightness: {full:.1} -> {small:.1}"
        );
    }

    /// The three maps must agree on where the glass is: a pane that is smooth in
    /// `<style>` but matte in the metal/rough map would not reflect.
    #[test]
    fn glass_indices_line_up_between_the_maps() {
        let tex = bake_facade(FacadeStyle::Apartment, 0);
        let n = tex.channel_size;
        let inv = 1.0 / n as f32;
        let mut checked = 0;
        for y in 0..n {
            for x in 0..n {
                let s = sample(
                    FacadeStyle::Apartment,
                    0,
                    (x as f32 + 0.5) * inv,
                    (y as f32 + 0.5) * inv,
                );
                let i = ((y * n + x) * 4) as usize;
                assert_eq!(tex.metallic_roughness[i + 1], to_u8(s.roughness));
                assert_eq!(tex.emissive[i], to_u8(s.emissive[0]));
                checked += 1;
            }
        }
        assert_eq!(checked, (n * n) as usize);
    }

    /// A lit pane must bake into a *room*, not a glowing rectangle: a hot spot,
    /// a broad fill and an occluded edge. The old 512² mask was a symmetric
    /// radial falloff, which is exactly the sticker look R3.11's rewrite was
    /// meant to avoid.
    #[test]
    fn a_lit_pane_is_a_room_not_a_sticker() {
        for style in STYLES {
            let spec = style.window();
            let seed = style as u32 * 104_729;
            let wall = test_wall();
            let mut checked = 0;
            // The lamp position is drawn per pane, so "off-centre" is a
            // property of the facade, not of any single pane: a pane whose
            // lamp happens to land near the middle is not a bug. These
            // accumulators carry that claim across every pane of the style.
            let mut off_centre_sum = 0.0f32;
            let (mut source_x0, mut source_x1) = (f32::MAX, f32::MIN);
            for bay in 0..BAYS_PER_TILE {
                for floor in 0..FLOORS_PER_TILE {
                    for pane_index in 0..spec.panes {
                        let center = (pane_index as f32 + 0.5) / spec.panes as f32;
                        let pane = pane_at(center, 0.5, spec.panes, bay, floor, spec.lit, seed);
                        // A drawn curtain is a soft glow on purpose: it hides
                        // the lamp, so the hot-spot assertions below only make
                        // sense on an open pane.
                        if !pane.lit || pane.curtain > 0.0 {
                            continue;
                        }
                        let mut peak = 0.0f32;
                        let mut sum = 0.0f32;
                        let mut cx = 0.0f32;
                        let mut cy = 0.0f32;
                        let mut n = 0.0f32;
                        for iy in 0..=20 {
                            for ix in 0..=20 {
                                let px = 0.08 + 0.84 * ix as f32 / 20.0;
                                let py = 0.06 + 0.88 * iy as f32 / 20.0;
                                let s = window_surface(style, &wall, &pane, px, py, seed);
                                let l = luminance(s.emissive);
                                peak = peak.max(l);
                                sum += l;
                                cx += l * px;
                                cy += l * py;
                                n += 1.0;
                            }
                        }
                        let mean = sum / n;
                        assert!(
                            peak > 0.35,
                            "{}: lit pane peaks at {peak:.2}",
                            style.label()
                        );
                        assert!(
                            mean < 0.62 * peak,
                            "{}: lit pane is flat (mean {mean:.2} vs peak {peak:.2})",
                            style.label()
                        );
                        // The reveal eats the light: a glass corner is much
                        // darker than the lamp, or the pane ends in a glowing
                        // rectangle.
                        let corner = window_surface(style, &wall, &pane, 0.10, 0.10, seed);
                        let corner_l = luminance(corner.emissive);
                        assert!(
                            corner_l < 0.30 * peak,
                            "{}: glass edge glows like the lamp ({corner_l:.2} vs {peak:.2})",
                            style.label()
                        );
                        // The glow sits where the lamp is, not at the pane's
                        // midpoint: that is what makes it a room with a light
                        // in it rather than a lit surface. Tolerance covers the
                        // fill and the lintel pulling the centroid a little.
                        let (cx, cy) = (cx / sum.max(1e-6), cy / sum.max(1e-6));
                        let to_source =
                            ((cx - pane.source.0).powi(2) + (cy - pane.source.1).powi(2)).sqrt();
                        assert!(
                            to_source < 0.20,
                            "{}: the glow ignores the lamp (centroid {cx:.2}, {cy:.2} vs source {:.2}, {:.2})",
                            style.label(),
                            pane.source.0,
                            pane.source.1
                        );
                        off_centre_sum +=
                            ((pane.source.0 - 0.5).powi(2) + (pane.source.1 - 0.5).powi(2)).sqrt();
                        source_x0 = source_x0.min(pane.source.0);
                        source_x1 = source_x1.max(pane.source.0);
                        checked += 1;
                    }
                }
            }
            assert!(checked > 0, "{}: no open lit pane to test", style.label());
            let off_centre = off_centre_sum / checked as f32;
            assert!(
                off_centre > 0.10,
                "{}: every lamp sits near the pane centre (mean {off_centre:.2})",
                style.label()
            );
            assert!(
                source_x1 - source_x0 > 0.15,
                "{}: the lamp never moves sideways (x {source_x0:.2}..{source_x1:.2})",
                style.label()
            );
        }
    }

    /// A lit room wraps its light around the mullion. Without this the frame is
    /// a black bar pasted across a glowing pane.
    #[test]
    fn a_lit_room_wraps_its_light_around_the_mullion() {
        let style = FacadeStyle::OldCity;
        let spec = style.window();
        let seed = style as u32 * 104_729;
        let wall = test_wall();
        let mut checked = 0;
        for bay in 0..BAYS_PER_TILE {
            for floor in 0..FLOORS_PER_TILE {
                for pane_index in 0..spec.panes {
                    let center = (pane_index as f32 + 0.5) / spec.panes as f32;
                    let pane = pane_at(center, 0.5, spec.panes, bay, floor, spec.lit, seed);
                    if !pane.lit {
                        continue;
                    }
                    // The mullion is the same pane with the glass mask off: its
                    // light comes from the room behind it, so it must share the
                    // room's colour and brightness.
                    let mullion_pane = Pane {
                        glass: false,
                        ..pane
                    };
                    let mullion = window_surface(style, &wall, &mullion_pane, 0.0, 0.5, seed);
                    let mullion_l = luminance(mullion.emissive);
                    // Compare against the pane's *brightest* glass, not its
                    // centre: the lamp is off-centre, so a fixed sample can land
                    // in a dim corner and make any frame look bright. The claim
                    // is that no part of the frame out-glows the room.
                    let mut glass_peak = 0.0f32;
                    for iy in 0..=8 {
                        for ix in 0..=8 {
                            let px = 0.02 + 0.96 * ix as f32 / 8.0;
                            let py = 0.02 + 0.96 * iy as f32 / 8.0;
                            let s = window_surface(style, &wall, &pane, px, py, seed);
                            glass_peak = glass_peak.max(luminance(s.emissive));
                        }
                    }
                    assert!(
                        mullion_l > 0.01,
                        "a mullion next to a lit room must not be black ({mullion_l:.3})"
                    );
                    assert!(
                        mullion_l < glass_peak * 0.5,
                        "the mullion is lit like the glass ({mullion_l:.3} vs peak {glass_peak:.3})"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no lit pane to test");
    }

    /// The sill under a lit window catches the spill; a black sill under a
    /// glowing pane is what makes the window look pasted on the wall.
    #[test]
    fn a_lit_room_spills_onto_its_sill() {
        for style in STYLES {
            let spec = style.window();
            let seed = style as u32 * 104_729;
            let mut found = false;
            for bay in 0..BAYS_PER_TILE {
                for floor in 0..FLOORS_PER_TILE {
                    for pane_index in 0..spec.panes {
                        let center = (pane_index as f32 + 0.5) / spec.panes as f32;
                        let pane = pane_at(center, 0.5, spec.panes, bay, floor, spec.lit, seed);
                        if !pane.lit {
                            continue;
                        }
                        let wander = (hash01(bay, floor, seed ^ 0x51ed) - 0.5) * 0.06;
                        let fx = 0.5 + wander + (center - 0.5) * spec.width;
                        let u = (bay as f32 + fx) / BAYS_PER_TILE as f32;
                        let v = (floor as f32 + spec.bottom + 0.012) / FLOORS_PER_TILE as f32;
                        let sill = sample(style, 0, u, v);
                        let l = luminance(sill.emissive);
                        assert!(
                            l > 0.004,
                            "{}: the sill under a lit window is black ({l:.4})",
                            style.label()
                        );
                        found = true;
                        break;
                    }
                    if found {
                        break;
                    }
                }
                if found {
                    break;
                }
            }
            assert!(found, "{}: no lit pane to test", style.label());
        }
    }

    /// The C light selector must be deterministic and stay inside the facade:
    /// a bay/floor that does not exist is a lamp nobody can see, and a
    /// non-deterministic pick would make chunks flicker as they stream.
    #[test]
    fn the_street_window_is_deterministic_and_inside_the_facade() {
        for style in STYLES {
            let a = street_window(style, 0, 8, 6);
            let b = street_window(style, 0, 8, 6);
            assert_eq!(
                a,
                b,
                "{}: the window pick is not deterministic",
                style.label()
            );
            let window = a.unwrap_or_else(|| panic!("{}: no open lit window", style.label()));
            assert!(window.bay < 8, "{}: bay out of range", style.label());
            assert!(window.floor < 6, "{}: floor out of range", style.label());
            assert!(
                (0.0..=1.0).contains(&window.fx),
                "{}: fx {} outside the bay",
                style.label(),
                window.fx
            );
            assert!(
                (0.0..1.0).contains(&window.fy),
                "{}: fy {} outside the floor",
                style.label(),
                window.fy
            );
            assert!(window.glow > 0.0);
        }
    }

    fn test_wall() -> Surface {
        Surface {
            albedo: [0.42, 0.39, 0.35],
            roughness: 0.9,
            metallic: 0.0,
            emissive: [0.0; 3],
        }
    }

    fn luminance(rgb: [f32; 3]) -> f32 {
        rgb[0].max(rgb[1]).max(rgb[2])
    }
}
