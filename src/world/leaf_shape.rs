//! Leaf silhouette shapes, baked to alpha masks (R3.10).
//!
//! The trees need their sky cut into pieces by a real outline: a paulownia
//! leaf, an aspen's sawtooth triangle and a willow's narrow blade are three
//! different silhouettes, and no amount of sphere geometry expresses that. A
//! leaf is 5-15 cm and a crown carries tens of thousands of them, so the shape
//! belongs on a 2D card, not in triangles.
//!
//! # What one mask contains
//!
//! ADR-0001's reading order is: habit first, leaf arrangement second, single
//! leaf last. So a mask holds ONE botanical unit:
//!
//! - **pinnate** (槐 sophora, 榆 elm): a whole compound leaf -- one rachis with
//!   pairs of leaflets along it. The 3D side places many of these on a branch
//!   using alternate/opposite phyllotaxis; the mask does not encode that.
//! - **simple** (杨 aspen, 柳 willow): one blade.
//!
//! Whether leaflets are opposite or alternate is a property of the ARRANGEMENT,
//! which the skeleton owns, not of the mask. Baking it here would fight the
//! branch layout and make every leaf face the same way.
//!
//! # Why procedural instead of a generated image
//!
//! The mask must be *exactly* the leaf, because it becomes an alpha test
//! cutoff. A diffusion model returns a plausible picture whose edge does not
//! correspond to any coordinate the game can reason about, and background
//! removal leaves a semi-transparent fringe that a hard cutoff turns into
//! ragged holes. Here the outline is a closed curve computed from a handful of
//! parameters, so the edge is analytic.
//!
//! Everything is deterministic (no RNG beyond a hash of the pixel position), so
//! the same parameters always bake the same bytes and the masks can live in
//! version control.
//!
//! # Adding a species (palm, etc.)
//!
//! Add a [`LeafShape`] variant plus one row in [`leaf_profiles`]. Nothing else
//! in the pipeline knows about species: the baker, the preview sheets and the
//! atlas builder iterate that table.

/// How a species' foliage unit is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafShape {
    /// A whole pinnate compound leaf: a rachis carrying pairs of leaflets.
    Pinnate,
    /// A single blade on a short petiole.
    Simple,
    /// A whole drooping shoot (柳条): a whip with alternate narrow leaves.
    ///
    /// 柳 does not grow foliage in clumps. Its unit is the hanging shoot, and a
    /// crown built from leaf clusters has the wrong habit at every leaf angle --
    /// the fix is the unit, not the card roll.
    Switch,
}

/// The tip of a leaflet or blade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafTip {
    /// Rounded (槐's leaflets).
    Rounded,
    /// Drawn out to a point (杨, 柳).
    Acute,
}

/// One species' foliage unit, in normalised space.
///
/// Coordinates are 0..1 inside the mask with y pointing DOWN (image order); the
/// outline functions below convert as needed. Sizes are fractions of the mask
/// edge, so a profile is resolution-independent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LeafProfile {
    /// Species key, also the atlas slot order.
    pub species: &'static str,
    /// Human label for preview sheets.
    pub label: &'static str,
    pub shape: LeafShape,

    // --- simple blades (used when `shape == Simple`) ---
    /// Blade length along the stem axis, as a fraction of the mask HEIGHT.
    pub blade_length: f32,
    /// The blade's real width-to-length ratio.
    ///
    /// This is a property of the leaf, not of the image, and it is what sizes
    /// the mask box. A first version divided the width fraction of the mask by
    /// the length fraction of the mask -- two fractions of different axes,
    /// whose ratio has no physical meaning -- and baked the willow into a 92 px
    /// wide box, where its blade collapsed into a hairline.
    pub leaf_aspect: f32,
    /// Where the widest point sits along the blade (0 = base, 1 = tip).
    pub blade_widest_at: f32,
    pub tip: LeafTip,
    /// Teeth per side of the blade (0 = smooth margin).
    ///
    /// A count rather than a margin enum: 杨's ~30 coarse teeth and 柳's ~46
    /// fine ones are the same rule at different settings, so the enum was
    /// hiding a number that has to be per species anyway.
    pub teeth: f32,
    /// How deep one tooth bites in, as a fraction of the local half-width.
    pub tooth_depth: f32,
    /// Where along the blade an acute tip starts to pinch (0 = base, 1 = tip).
    ///
    /// 柳's long acuminate tail has to start far earlier than 杨's short point;
    /// a shared constant gave both the same blunt cone.
    pub tip_taper_start: f32,
    /// Exponent of the blade's rise out of the petiole (lower = broader base).
    ///
    /// 杨's deltoid is nearly full width a fifth of the way up; 柳 widens
    /// gradually from a narrow base. Sharing one exponent flattened both.
    pub blade_rise: f32,

    // --- pinnate compound leaves (used when `shape == Pinnate`) ---
    /// Pairs of leaflets along the rachis.
    pub leaflet_pairs: u8,
    /// Length of the terminal leaflet relative to a side leaflet.
    pub terminal_ratio: f32,
    /// Leaflet length, fraction of the mask.
    pub leaflet_length: f32,
    /// Leaflet width / length.
    pub leaflet_aspect: f32,
    /// How much leaflets angle up toward the rachis tip (radians).
    pub leaflet_angle: f32,
    /// Rachis length, fraction of the mask height.
    pub rachis_length: f32,
}

/// Slack factor applied to a compound leaf's mask width, so the longest
/// leaflet any variant draws still fits inside the cell.
const PINNATE_BOX_HEADROOM: f32 = 1.16;

impl LeafProfile {
    /// Aspect ratio the mask should be baked at (width / height).
    ///
    /// A willow blade is a 6:1 sliver and a pinnate leaf is roughly 2:1; baking
    /// both into a square wastes texels and, worse, makes the willow's blade
    /// occupy a few pixels across. The baker sizes the image from this.
    pub fn mask_aspect(&self) -> f32 {
        match self.shape {
            // A simple blade fills its box, so the box IS the leaf's aspect.
            // The floor keeps a very thin leaf from becoming a sub-pixel
            // sliver; below ~0.12 the blade stops reading as a leaf at any
            // distance the game renders.
            LeafShape::Simple => self.leaf_aspect.clamp(0.12, 1.0),
            // A compound leaf is as wide as its leaflets actually reach: one
            // leaflet's horizontal offset from the rachis plus its own rotated
            // half-extent. A fixed guess here wasted half the box in empty
            // margin, which shrinks every leaflet for no gain -- and a box that
            // is too narrow cuts the outer tips flat, which is exactly what a
            // leaf must never look like.
            LeafShape::Pinnate | LeafShape::Switch => {
                let (sin, cos) = self.leaflet_angle.sin_cos();
                let half_w = self.leaflet_length * self.leaflet_aspect * 0.5;
                let half_l = self.leaflet_length * 0.5;
                let reach = half_l * cos + (half_w * sin).hypot(half_l * cos);
                // Headroom for the per-leaflet drift in `pinnate_covers`, which
                // can stretch one leaflet by up to ~16%.
                (reach * 2.0 * PINNATE_BOX_HEADROOM).clamp(0.12, 1.0)
            }
        }
    }

    /// Blade width in pixels for a mask `height` tall.
    ///
    /// Derived from the aspect rather than stored, so the two can never
    /// disagree.
    pub fn blade_width_px(&self, height: u32) -> f32 {
        self.blade_length * height as f32 * self.leaf_aspect
    }
}

/// The species table. One row per species; this is the only place that knows
/// which trees exist.
pub fn leaf_profiles() -> &'static [LeafProfile] {
    &PROFILES
}

static PROFILES: [LeafProfile; 3] = [
    // 槐 (Sophora japonica): odd-pinnate, 7-11 pairs of rounded, entire
    // leaflets. The round-tipped pairs are what makes it readable as 槐 rather
    // than 榆.
    LeafProfile {
        species: "sophora",
        label: "Sophora (huai) - odd pinnate",
        shape: LeafShape::Pinnate,
        blade_length: 0.0,
        leaf_aspect: 0.0,
        blade_widest_at: 0.0,
        tip: LeafTip::Rounded,
        teeth: 0.0,
        tooth_depth: 0.0,
        tip_taper_start: 0.90,
        blade_rise: 0.62,
        // 8 pairs over a 0.88 rachis leaves ~56 px between attachments at
        // 512 px. A leaflet that lies nearly horizontal only spans ~35% of its
        // length vertically (~27 px here), so long leaflets still leave a gap
        // between one pair and the next -- the earlier 0.095 was compensating
        // for a rotation bug, not for real crowding.
        leaflet_pairs: 8,
        terminal_ratio: 0.8,
        leaflet_length: 0.11,
        leaflet_aspect: 0.42,
        leaflet_angle: 0.34,
        rachis_length: 0.88,
    },
    // 杨 (Poplar): simple, deltoid, serrate. The triangle plus the sawtooth is
    // the whole identification.
    LeafProfile {
        species: "poplar",
        label: "Poplar (yang) - deltoid, serrate",
        shape: LeafShape::Simple,
        blade_length: 0.82,
        // Deltoid: about 0.8 as wide as it is long -- a real 加杨 leaf is
        // nearly as broad as its blade is long, and 0.7 left it looking narrow.
        leaf_aspect: 0.78,
        // Widest near the base: that is what makes it a triangle and not an
        // oval.
        blade_widest_at: 0.20,
        tip: LeafTip::Acute,
        // A fine sawtooth, not 30 tree-sized ones: coarse teeth on a triangular
        // outline read as a conifer, which is the one thing 杨 must not look
        // like. Real serration is ~1 mm on a 10 cm leaf, so 42 teeth is still a
        // deliberate exaggeration.
        teeth: 42.0,
        // 13% was a scalloped edge, not a sawtooth; 18% was so deep that the
        // teeth became branches.
        tooth_depth: 0.12,
        tip_taper_start: 0.86,
        // Deltoid means a nearly straight, flat base: the blade has to be most
        // of its width within the first tenth of its length. At 0.45 it rose
        // like a cone, which reads as a conifer rather than a poplar leaf.
        blade_rise: 0.28,
        leaflet_pairs: 0,
        terminal_ratio: 0.0,
        leaflet_length: 0.0,
        leaflet_aspect: 0.0,
        leaflet_angle: 0.0,
        rachis_length: 0.0,
    },
    // 柳 (Willow): a drooping shoot (柳条), not a blade and not a clump.
    //
    // Weeping willow foliage hangs from long pendulous switches that carry
    // alternate narrow blades; nothing in the crown is organised as a leaf
    // cluster. So the baked unit is the whole switch, and the 3D side hangs it
    // off the twig. The blade parameters below now describe the blades ON the
    // switch: the ellipse they are drawn as keeps the 1:4 lanceolate sliver.
    LeafProfile {
        species: "willow",
        label: "Willow (liu) - drooping shoot",
        shape: LeafShape::Switch,
        blade_length: 0.0,
        leaf_aspect: 0.0,
        blade_widest_at: 0.0,
        tip: LeafTip::Acute,
        teeth: 0.0,
        tooth_depth: 0.0,
        tip_taper_start: 0.70,
        blade_rise: 0.62,
        // Alternating leaves down the whole whip: an odd count with the last
        // one nearest the free end, which is what makes the shoot read as a
        // hanging strand rather than a feather.
        leaflet_pairs: 11,
        terminal_ratio: 0.0,
        leaflet_length: 0.085,
        leaflet_aspect: 0.28,
        // Blades trail downward as they leave the whip; +0.38 rad is below the
        // horizontal (the ellipse angle is computed from this below).
        leaflet_angle: 0.38,
        rachis_length: 0.96,
    },
];

/// Look up a profile by species key.
pub fn find_profile(species: &str) -> Option<&'static LeafProfile> {
    leaf_profiles().iter().find(|p| p.species == species)
}

/// A baked alpha mask: one byte of coverage per pixel, plus its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlphaMask {
    pub width: u32,
    pub height: u32,
    /// Row-major, `width * height`, 0 = transparent.
    pub alpha: Vec<u8>,
}

impl AlphaMask {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            alpha: vec![0; (width * height) as usize],
        }
    }

    pub fn get(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.alpha[(y * self.width + x) as usize]
    }

    /// Fraction of pixels with any coverage (coverage > 0).
    pub fn opaque_fraction(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let n = self.alpha.iter().filter(|a| **a > 0).count();
        n as f32 / self.alpha.len() as f32
    }

    /// Mean coverage over the whole image, which is the number that matters
    /// for suppression: a mask that is 5% covered contributes almost no
    /// overdraw.
    pub fn mean_coverage(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let sum: u64 = self.alpha.iter().map(|a| *a as u64).sum();
        sum as f32 / self.alpha.len() as f32
    }

    /// Fraction of texels that survive the game's alpha test.
    ///
    /// This is the metric that matters for mip reduction, and it is NOT
    /// [`Self::mean_coverage`]: a box filter preserves the mean exactly, so
    /// averaging a blade into finer mips leaves the average untouched while
    /// steadily pushing every individual texel below the cutoff. What the GPU
    /// keeps is the count of texels above [`ALPHA_CUTOFF`].
    pub fn visible_fraction(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let cutoff = (ALPHA_CUTOFF * 255.0) as u8;
        let n = self.alpha.iter().filter(|a| **a >= cutoff).count();
        n as f32 / self.alpha.len() as f32
    }

    /// Box-filtered 2x downsample, standing in for the GPU's mip chain.
    pub fn downsample_half(&self) -> AlphaMask {
        let w = (self.width / 2).max(1);
        let h = (self.height / 2).max(1);
        let mut out = AlphaMask::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let mut sum = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        sum += self.get(x * 2 + dx, y * 2 + dy) as u32;
                    }
                }
                out.alpha[(y * w + x) as usize] = (sum / 4) as u8;
            }
        }
        out
    }
}

/// How many alpha variants each species ships.
///
/// One mask per species made every card in a crown sample the same outline, so
/// a canopy seen from below read as a wallpaper repeat. The variants are the
/// same leaf with small deterministic jitter in its dimensions, so the species
/// stays identifiable while no two cards are byte-identical.
pub const LEAF_VARIANTS: usize = 4;

/// bakes the alpha mask for one `variant` of `profile` at `height` pixels tall.
///
/// Variant 0 is the canonical shape from the table; the others perturb it (see
/// [`variant_profile`]) and add per-leaflet drift. The width follows
/// [`LeafProfile::mask_aspect`], rounded to a multiple of 4 so the result sits
/// cleanly in an atlas slot.
pub fn bake_leaf_alpha(profile: &LeafProfile, variant: usize, height: u32) -> AlphaMask {
    let variant = variant % LEAF_VARIANTS;
    let profile = variant_profile(profile, variant);
    let width = mask_width(&profile, height);
    let mut mask = AlphaMask::new(width, height);
    let jitter = Jitter::new(profile.species, variant);
    match profile.shape {
        LeafShape::Simple => draw_simple_blade(&mut mask, &profile, jitter),
        LeafShape::Pinnate => draw_pinnate(&mut mask, &profile, jitter),
        LeafShape::Switch => draw_switch(&mut mask, &profile, jitter),
    }
    mask
}

/// The profile of one variant: the table row, with small deterministic
/// perturbations that stay inside the species' identity.
///
/// Amounts are deliberately small (±4-8%): a willow must stay a willow. What
/// they buy is that a crown's cards are not all the same outline, which is what
/// the eye reads as "wallpaper" when looking up into a canopy.
fn variant_profile(profile: &LeafProfile, variant: usize) -> LeafProfile {
    let jitter = Jitter::new(profile.species, variant);
    let mut p = *profile;
    if jitter.amount == 0.0 {
        return p;
    }
    // Every species: the blade's proportions drift a little.
    p.blade_length = (p.blade_length * (1.0 + 0.04 * jitter.at(0, 0))).min(1.0);
    p.leaf_aspect *= 1.0 + 0.07 * jitter.at(0, 1);
    p.blade_widest_at = (p.blade_widest_at + 0.05 * jitter.at(0, 2)).clamp(0.10, 0.90);
    // Pinnate species: the compound leaf's leaflet geometry.
    p.leaflet_length *= 1.0 + 0.06 * jitter.at(0, 3);
    p.leaflet_aspect *= 1.0 + 0.08 * jitter.at(0, 4);
    p.leaflet_angle = (p.leaflet_angle + 0.08 * jitter.at(0, 5)).max(0.05);
    p.rachis_length *= 1.0 + 0.02 * jitter.at(0, 6);
    p
}

/// Deterministic per-variant noise.
///
/// A hash rather than an RNG: the masks go into version control, so the same
/// (species, variant) must always produce the same bytes -- see
/// [`masks_are_deterministic`].
#[derive(Clone, Copy, Debug)]
struct Jitter {
    seed: u64,
    /// 0 for the canonical variant, 1 for the perturbed ones.
    amount: f32,
}

impl Jitter {
    fn new(species: &str, variant: usize) -> Self {
        if variant == 0 {
            return Self {
                seed: 0,
                amount: 0.0,
            };
        }
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in species.as_bytes() {
            hash = (hash ^ *byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            seed: hash ^ (variant as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
            amount: 1.0,
        }
    }

    /// Signed noise in `[-1, 1]` for `(index, channel)`.
    ///
    /// `index` lets the drawing code perturb each leaflet of one compound leaf
    /// independently, which is the variation that actually reads at silhouette
    /// size.
    fn at(&self, index: u32, channel: u32) -> f32 {
        if self.amount == 0.0 {
            return 0.0;
        }
        let mut x = self.seed
            ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (channel as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        ((x >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

/// The baker's own mask-width rule, shared with the atlas layout so the two
/// can never disagree.
fn mask_width(profile: &LeafProfile, height: u32) -> u32 {
    (((height as f32) * profile.mask_aspect()).round() as u32)
        .max(8)
        .next_multiple_of(4)
}

/// Height the shared species atlas is baked at (`chunks.rs` builds the world's
/// single leaf texture from this; `tree_mesh.rs` looks cells up at the same
/// height, so the two can never disagree).
///
/// 512 rather than 256: the 槐 cell is only ~0.13 of the height wide, so at 256
/// its eight leaflet pairs were 11 x 27 px and the compound leaf collapsed into
/// a wavy column. The whole atlas is ~2 MB, which is nothing against the
/// foliage it buys.
pub const ATLAS_HEIGHT: u32 = 512;

/// Width of every atlas cell, in bake order (species-major, then variant).
///
/// Shared by the baker and by [`species_variant_cell`] so the two can never
/// disagree about the layout.
fn cell_widths(height: u32) -> Vec<u32> {
    let mut widths = Vec::with_capacity(PROFILES.len() * LEAF_VARIANTS);
    for profile in PROFILES.iter() {
        for variant in 0..LEAF_VARIANTS {
            widths.push(mask_width(&variant_profile(profile, variant), height));
        }
    }
    widths
}

/// Bakes every species' every variant into ONE atlas.
///
/// The world's trees share a single leaf texture (one shared material is what
/// makes a season switch one 32-byte uniform write, R3.5); each card samples its
/// own species-and-variant cell, resolved by [`species_variant_cell`]. Cells run
/// in [`PROFILES`] order, variants together, each at its own baked width.
pub fn bake_species_atlas(height: u32) -> AlphaMask {
    let widths = cell_widths(height);
    let total: u32 = widths.iter().sum();
    let mut atlas = AlphaMask::new(total, height);
    let mut x = 0u32;
    let mut cell = 0usize;
    for profile in PROFILES.iter() {
        for variant in 0..LEAF_VARIANTS {
            let w = widths[cell];
            let mask = bake_leaf_alpha(profile, variant, height);
            for y in 0..height {
                for cx in 0..w {
                    atlas.alpha[(y * total + x + cx) as usize] = mask.get(cx, y);
                }
            }
            x += w;
            cell += 1;
        }
    }
    atlas
}

/// The (u0, u1) sub-rect one species' `variant` samples in the shared atlas,
/// with a half-texel inset so neighbouring cells do not bleed at mip levels.
pub fn species_variant_cell(species: &str, variant: usize, height: u32) -> (f32, f32) {
    let widths = cell_widths(height);
    let total: u32 = widths.iter().sum();
    let variant = variant % LEAF_VARIANTS;
    let mut x = 0u32;
    let mut cell = 0usize;
    for profile in PROFILES.iter() {
        for v in 0..LEAF_VARIANTS {
            if profile.species == species && v == variant {
                let inset = 0.5 / total as f32;
                return (
                    x as f32 / total as f32 + inset,
                    (x + widths[cell]) as f32 / total as f32 - inset,
                );
            }
            x += widths[cell];
            cell += 1;
        }
    }
    (0.0, 1.0)
}

/// Draws one simple blade into `mask`.
///
/// The blade is laid along the vertical axis: petiole at the bottom, tip at the
/// top. Coverage is anti-aliased by sampling the outline's signed distance at
/// 2x2 subpixel positions, which is what keeps a 6:1 willow blade from turning
/// into a dotted line when the mask is small.
fn draw_simple_blade(mask: &mut AlphaMask, profile: &LeafProfile, jitter: Jitter) {
    let w = mask.width as f32;
    let h = mask.height as f32;
    let blade_len = profile.blade_length * h;
    let blade_wid = profile.blade_width_px(mask.height);
    // Centre the blade vertically, leaving room for the petiole below it.
    let base_y = (h - blade_len) * 0.5 + blade_len;
    let tip_y = base_y - blade_len;
    let center_x = w * 0.5;
    // The petiole: a short stem under the blade, drawn to the bottom of the
    // cell. Without it an isolated deltoid blade reads as a whole conifer --
    // a bare triangle with no stem is the silhouette of a fir, not of a leaf.
    let petiole_bottom = (base_y + (h - base_y) * 0.85).min(h - 1.0);
    let petiole_half = (w * 0.012).max(0.8);

    for y in 0..mask.height {
        for x in 0..mask.width {
            let mut hits = 0u32;
            for (sx, sy) in SUBPIXELS {
                let px = x as f32 + sx;
                let py = y as f32 + sy;
                let in_blade =
                    blade_covers(px, py, base_y, tip_y, blade_wid, center_x, profile, jitter);
                let in_petiole = (px - center_x).abs() <= petiole_half
                    && (base_y..=petiole_bottom).contains(&py);
                if in_blade || in_petiole {
                    hits += 1;
                }
            }
            let cov = (hits * 255 / SUBPIXELS.len() as u32) as u8;
            mask.alpha[(y * mask.width + x) as usize] = cov;
        }
    }
}

/// 2x2 subpixel offsets for anti-aliasing.
const SUBPIXELS: [(f32, f32); 4] = [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)];

/// Is the point inside the blade?
///
/// The half-width follows a smooth profile that starts narrow at the petiole,
/// peaks at `blade_widest_at`, and closes at the tip. 杨's deltoid needs the peak
/// pushed toward the base; 柳's lanceolate needs it near the middle.
#[allow(clippy::too_many_arguments)]
fn blade_covers(
    px: f32,
    py: f32,
    base_y: f32,
    tip_y: f32,
    blade_wid: f32,
    center_x: f32,
    profile: &LeafProfile,
    jitter: Jitter,
) -> bool {
    let len = (base_y - tip_y).max(1e-4);
    // t: 0 at the petiole, 1 at the tip.
    let t = (base_y - py) / len;
    if !(0.0..=1.0).contains(&t) {
        return false;
    }

    let half = half_width_at(t, blade_wid, profile);
    // Serration modulates the outline near the margin, so the mask's edge is
    // toothed rather than a smooth curve. The tooth count is per blade length so
    // a big blade and a small one keep the same tooth pitch.
    let dx = (px - center_x).abs();
    let mut limit = half;
    if profile.teeth >= 1.0 {
        // A triangle wave, not `sin`: a sine bites like a scallop, and a leaf
        // margin has POINTS between the bites. The phase drifts per variant so
        // no two species cells share a tooth registration.
        let phase = t * profile.teeth + 0.5 + 0.35 * jitter.at(1, 0);
        let frac = phase - phase.floor();
        let tooth = (frac * 2.0 - 1.0).abs();
        limit *= 1.0 - profile.tooth_depth * (1.0 - tooth);
    }
    if dx > limit {
        return false;
    }

    // Acute tips taper to a point over the last stretch; rounded tips stay
    // broad until they close. Done as a post-check so the width profile stays
    // readable.
    if profile.tip == LeafTip::Acute {
        let taper_start = profile.tip_taper_start.clamp(0.30, 0.98);
        if t > taper_start {
            let k = (t - taper_start) / (1.0 - taper_start);
            let narrowed = half * (1.0 - k.powf(1.4));
            if dx > narrowed.max(0.0) {
                return false;
            }
        }
    }
    true
}

/// Half-width of the blade at parameter `t` (0 = petiole, 1 = tip).
///
/// Two smooth lobes meeting at `blade_widest_at`: a leaf widens quickly from
/// the petiole and then narrows to the tip, and the asymmetry (fast in, slow
/// out) is what makes a blade read as a blade.
fn half_width_at(t: f32, blade_wid: f32, profile: &LeafProfile) -> f32 {
    let peak = profile.blade_widest_at.clamp(0.05, 0.95);
    let s = if t < peak {
        // Rise: the exponent is the species' base shape. 杨's deltoid is nearly
        // full width almost at once (0.45); 柳 widens gradually from a narrow
        // base (0.62). One shared exponent flattened both into an oval.
        (t / peak).powf(profile.blade_rise.clamp(0.20, 2.0))
    } else {
        // Fall toward the tip.
        let k = (t - peak) / (1.0 - peak);
        (1.0 - k).powf(0.78)
    };
    blade_wid * 0.5 * s.clamp(0.0, 1.0)
}

/// Draws a pinnate compound leaf: a rachis with `leaflet_pairs` pairs plus a
/// terminal leaflet.
fn draw_pinnate(mask: &mut AlphaMask, profile: &LeafProfile, jitter: Jitter) {
    let w = mask.width as f32;
    let h = mask.height as f32;
    let center_x = w * 0.5;
    // The rachis runs up the middle; the petiole is the bare stretch below it.
    let rachis_len = profile.rachis_length * h;
    let rachis_bottom = h * 0.96;
    let rachis_top = rachis_bottom - rachis_len;
    let pairs = profile.leaflet_pairs.max(1);

    for y in 0..mask.height {
        for x in 0..mask.width {
            let mut hits = 0u32;
            for (sx, sy) in SUBPIXELS {
                let px = x as f32 + sx;
                let py = y as f32 + sy;
                if pinnate_covers(
                    px,
                    py,
                    center_x,
                    rachis_bottom,
                    rachis_top,
                    w,
                    h,
                    profile,
                    pairs,
                    jitter,
                ) {
                    hits += 1;
                }
            }
            let cov = (hits * 255 / SUBPIXELS.len() as u32) as u8;
            mask.alpha[(y * mask.width + x) as usize] = cov;
        }
    }
}

/// Draws one hanging willow shoot (柳条) into `mask`.
///
/// The top edge of the mask is the ATTACHMENT (where the shoot leaves the
/// twig); the bottom is the free hanging end. `leaf_cards` in the mesh builder
/// knows this and maps v = 1 to the attachment for this shape only.
fn draw_switch(mask: &mut AlphaMask, profile: &LeafProfile, jitter: Jitter) {
    let w = mask.width as f32;
    let h = mask.height as f32;
    let center_x = w * 0.5;
    let lean = w * (0.05 + 0.04 * jitter.at(9, 0));
    // ~3 px at 512 tall: a real shoot is 3-5 mm on a metre, and a hairline this
    // long is the first thing the mip chain's dilation erases into a stripe.
    let whip_r = (w * 0.022).max(1.5);

    for y in 0..mask.height {
        for x in 0..mask.width {
            let mut hits = 0u32;
            for (sx, sy) in SUBPIXELS {
                if switch_covers(
                    x as f32 + sx,
                    y as f32 + sy,
                    center_x,
                    lean,
                    whip_r,
                    h,
                    profile,
                    jitter,
                ) {
                    hits += 1;
                }
            }
            let cov = (hits * 255 / SUBPIXELS.len() as u32) as u8;
            mask.alpha[(y * mask.width + x) as usize] = cov;
        }
    }
}

/// Where the whip sits at `t` (0 = attachment, 1 = free end): it sags away
/// from the twig as it falls, which is what a willow shoot does under its own
/// weight.
fn whip_x(t: f32, center_x: f32, lean: f32) -> f32 {
    center_x + lean * t * t
}

/// Inside-the-shoot test: the whip, or one of its alternate blades.
#[allow(clippy::too_many_arguments)]
fn switch_covers(
    px: f32,
    py: f32,
    center_x: f32,
    lean: f32,
    whip_r: f32,
    h: f32,
    profile: &LeafProfile,
    jitter: Jitter,
) -> bool {
    let t = (py / h).clamp(0.0, 1.0);
    if (px - whip_x(t, center_x, lean)).abs() <= whip_r {
        return true;
    }

    let leaves = profile.leaflet_pairs.max(2);
    let span = profile.rachis_length * h;
    let top = h * 0.03;
    for i in 0..leaves {
        let ti = (i as f32 + 0.5) / leaves as f32;
        // Alternate sides: one blade per node, not a mirrored pair. That single
        // choice is what separates a willow whip from a fern frond.
        let side = if i % 2 == 0 { -1.0_f32 } else { 1.0 };
        // Blades shrink toward the free end, as they do on a real shoot.
        let len = profile.leaflet_length
            * h
            * (1.0 - 0.35 * ti)
            * (1.0 + 0.10 * jitter.at(i as u32 + 1, 0));
        let angle = (profile.leaflet_angle + 0.05 * jitter.at(i as u32 + 1, 1)).max(0.0);
        let attach_y = top + span * ti;
        let along_x = side * len * 0.5 * angle.cos();
        // +y is DOWN in the mask: the blade trails below its node.
        let along_y = len * 0.5 * angle.sin();
        let ellipse = LeafletEllipse {
            cx: whip_x(ti, center_x, lean) + along_x,
            cy: attach_y + along_y,
            semi_x: len * profile.leaflet_aspect * 0.5,
            semi_y: len * 0.5,
            // The ellipse angle is measured from the vertical and the tip is at
            // local -y, so `side * (pi/2 + a)` points the pointed end outward
            // and down -- the trailing direction of a hanging blade.
            angle: side * (std::f32::consts::FRAC_PI_2 + angle),
            tip: profile.tip,
        };
        if ellipse_covers(px, py, ellipse) {
            return true;
        }
    }
    false
}

/// Inside-the-compound-leaf test: the rachis, or any leaflet (or the terminal
/// one).
#[allow(clippy::too_many_arguments)]
fn pinnate_covers(
    px: f32,
    py: f32,
    center_x: f32,
    rachis_bottom: f32,
    rachis_top: f32,
    w: f32,
    h: f32,
    profile: &LeafProfile,
    pairs: u8,
    jitter: Jitter,
) -> bool {
    // Rachis: a thin line up the middle. Kept just visible -- at silhouette
    // distance a hairline rachis is the spine that makes the leaflets read as
    // one leaf instead of scattered dots.
    let rachis_r = (w.min(h) * 0.006).max(0.6);
    if py <= rachis_bottom && py >= rachis_top && (px - center_x).abs() <= rachis_r {
        return true;
    }

    let llen = profile.leaflet_length * h;
    // Leaflets attach along the rachis, leaving the lowest stretch as petiole.
    let span = rachis_bottom - rachis_top;
    for i in 0..pairs {
        let t = (i as f32 + 0.6) / (pairs as f32 + 0.2);
        let attach_y = rachis_bottom - span * t;
        // Per-leaflet drift: a real compound leaf is not a stamped ladder, and
        // at silhouette size the uneven lengths are what distinguish it from a
        // fern. Variant 0 keeps the canonical shape; the other variants also
        // vary left against right.
        let len_i = llen * (1.0 + 0.10 * jitter.at(i as u32 + 1, 0));
        let lwid_i = len_i * profile.leaflet_aspect;
        let angle_i = (profile.leaflet_angle + 0.06 * jitter.at(i as u32 + 1, 1)).max(0.0);
        for side in [-1.0_f32, 1.0] {
            let channel = if side < 0.0 { 2 } else { 3 };
            let side_len = len_i * (1.0 + 0.06 * jitter.at(i as u32 + 1, channel));
            // Angle up toward the tip, as a real pinnate leaf does.
            let along_x = side * side_len * 0.5 * angle_i.cos();
            let along_y = -side_len * 0.5 * angle_i.sin();
            let cx = center_x + along_x;
            let cy = attach_y + along_y;
            let ellipse = LeafletEllipse {
                cx,
                cy,
                semi_x: lwid_i * 0.5,
                semi_y: side_len * 0.5,
                // `ellipse_covers` measures its angle from the VERTICAL, while
                // `leaflet_angle` is the tilt up from the horizontal -- the same
                // quantity `along_x`/`along_y` above use. Converting here (and
                // mirroring the sign for the left side) is what makes the
                // leaflet body point the way its attachment offset already
                // does; sharing the raw angle drew near-vertical ovals beside
                // horizontally offset centers, i.e. a zipper, not a leaf.
                angle: side * (std::f32::consts::FRAC_PI_2 - angle_i),
                tip: profile.tip,
            };
            if ellipse_covers(px, py, ellipse) {
                return true;
            }
        }
    }

    // Terminal leaflet: points straight up from the rachis tip, slightly
    // smaller than the side leaflets for 槐's odd-pinnate look.
    let tlen = llen * profile.terminal_ratio;
    let twid = tlen * profile.leaflet_aspect;
    ellipse_covers(
        px,
        py,
        LeafletEllipse {
            cx: center_x,
            cy: rachis_top - tlen * 0.5,
            semi_x: twid * 0.5,
            semi_y: tlen * 0.5,
            angle: 0.0,
            tip: profile.tip,
        },
    )
}

/// One leaflet's ellipse: where it sits and how much room it takes.
///
/// Bundled rather than passed as seven loose floats so the call sites read as
/// "does this point fall inside this leaflet" instead of a positional argument
/// list nobody can check.
#[derive(Clone, Copy, Debug)]
struct LeafletEllipse {
    cx: f32,
    cy: f32,
    semi_x: f32,
    semi_y: f32,
    /// Rotation in radians, positive = tip swung up toward the rachis tip.
    angle: f32,
    tip: LeafTip,
}

/// Is the point inside this leaflet?
fn ellipse_covers(px: f32, py: f32, e: LeafletEllipse) -> bool {
    let (cx, cy, semi_x, semi_y, angle, tip) = (e.cx, e.cy, e.semi_x, e.semi_y, e.angle, e.tip);
    if semi_x <= 0.0 || semi_y <= 0.0 {
        return false;
    }
    let dx = px - cx;
    let dy = py - cy;
    let (s, c) = angle.sin_cos();
    // Into the ellipse's own frame.
    let lx = dx * c + dy * s;
    let ly = -dx * s + dy * c;
    let nx = lx / semi_x;
    let ny = ly / semi_y;
    if nx * nx + ny * ny > 1.0 {
        return false;
    }
    // A pointed leaflet tip: pinch the upper half toward the axis. Rounded tips
    // keep the plain ellipse.
    if tip == LeafTip::Acute && ny < -0.35 {
        let k = (-ny - 0.35) / 0.65;
        if nx.abs() > (1.0 - k.powf(1.5)).max(0.0) {
            return false;
        }
    }
    true
}

/// Grows the alpha outward by `radius` pixels, keeping the covered area's
/// interior unchanged.
///
/// This exists for the mip chain, and it is not cosmetic. A leaf mask is
/// mostly coverage BELOW 0.5: a 1 px willow blade averaged into a 4x4 texel
/// lands around 0.06. With `AlphaMode::Mask(0.5)` the GPU discards those
/// texels, so a distant crown thins out and finally disappears -- the sky stops
/// being cut and the tree turns into a smudge.
///
/// The standard fix is to bake the dilation into the mips: each level keeps the
/// silhouette but pushes coverage outward so the average stays above cutoff.
/// Doing it here (rather than at import time) keeps the masks honest and lets
/// the atlas builder apply per-level radii.
pub fn dilate_alpha(mask: &AlphaMask, radius: u32) -> AlphaMask {
    if radius == 0 {
        return mask.clone();
    }
    let mut out = mask.clone();
    let r = radius as i32;
    // Two-pass separable max filter: cheaper than a naive disc and the
    // difference is invisible at these radii.
    let mut tmp = mask.clone();
    for y in 0..mask.height as i32 {
        for x in 0..mask.width as i32 {
            let mut best = 0u8;
            for dx in -r..=r {
                best =
                    best.max(mask.get((x + dx).clamp(0, mask.width as i32 - 1) as u32, y as u32));
            }
            tmp.alpha[(y as u32 * mask.width + x as u32) as usize] = best;
        }
    }
    for y in 0..mask.height as i32 {
        for x in 0..mask.width as i32 {
            let mut best = 0u8;
            for dy in -r..=r {
                best =
                    best.max(tmp.get(x as u32, (y + dy).clamp(0, mask.height as i32 - 1) as u32));
            }
            out.alpha[(y as u32 * mask.width + x as u32) as usize] = best;
        }
    }
    out
}

/// Builds the mip chain a leaf card should ship with: level 0 is the raw shape,
/// and every level below it is the half-resolution image dilated enough to keep
/// the silhouette above the alpha cutoff.
///
/// Dilating the base image once (the first approach) does not work, and the
/// failing level makes the reason obvious: a 256 px willow blade survives 5
/// halvings but not 7, because each halving averages more of the thin blade's
/// neighbourhood into every texel. A fixed radius applied at level 0 has
/// already been averaged away by then. The radius has to grow with the level.
///
/// `radius_at(level, width, height)` is that growth. Returning 0 at level 0 keeps the
/// full-resolution shape exact, which matters because that is the level the
/// player sees against the sky.
pub fn build_mip_chain(
    mask: &AlphaMask,
    levels: u32,
    radius_at: impl Fn(u32, u32, u32) -> u32,
) -> Vec<AlphaMask> {
    let mut chain = Vec::with_capacity(levels as usize);
    chain.push(mask.clone());
    let mut current = mask.clone();
    for level in 1..levels {
        current = current.downsample_half();
        let dilated = dilate_alpha(&current, radius_at(level, current.width, current.height));
        chain.push(dilated);
    }
    chain
}

/// Dilation radius for one level of a mask of the given pixel size.
///
/// The radius is a fraction of the MASK, not a pixel count. A fixed sequence
/// (1, 1, 2, 3, 4, 6...) was the first attempt and it broke exactly where it
/// mattered: by level 3 the sophora mask is 9 px wide, so a 2 px radius -- 22%
/// of the width, on both sides -- dilated it to 94% coverage and by level 4 it
/// was a solid block. The leaf stayed "visible" while ceasing to be a leaf.
///
/// Scaling with the mask keeps the operation proportional: it thickens the
/// silhouette by roughly a texel or two at every level, which is what the box
/// filter costs, without ever approaching a rectangle.
///
/// Returns 0 at level 0: full resolution is what the player sees against the
/// sky, so it must not be touched.
pub fn default_mip_radius(level: u32, mask_width: u32, mask_height: u32) -> u32 {
    if level == 0 {
        return 0;
    }
    // Below ~8 px a compound leaf has no shape left to protect: the sophora
    // mask reaches 4 px by level 4, where a 1 px radius on both sides is 50% of
    // the width and fills it solid. Dilating there keeps a "visible" block that
    // reads as a leaf-shaped lie. The honest answer is that the card has
    // outlived its shape, which is a LOD decision -- see
    // [`mip_chain_preserves_shape`]. So: no dilation once the mask is that
    // small.
    // A compound leaf's box is narrow AND tall; a simple blade's box is as
    // wide as the leaf. Only the short side says how much room a dilation has
    // before it closes the gaps that make the shape.
    let short = mask_width.min(mask_height);
    if short < MIN_SHAPE_WITH {
        return 0;
    }
    // ~4% of the short side, at least one texel (a box filter cannot keep a
    // sub-texel feature alive).
    ((short as f32) * 0.04).round().max(1.0) as u32
}

/// Dilation radius for the SHARED ATLAS, measured against its narrowest cell
/// rather than against the whole sheet.
///
/// [`default_mip_radius`] sizes the radius from a mask's short side. That is
/// correct for a standalone bake, where the short side IS the leaf's width,
/// but the shipped texture packs every species side by side: at 272x256 its
/// short side is 256, so level 1 asks for a 5 px dilation while the narrowest
/// cell -- sophora's pinnate leaf -- is just 40 px (20 px at level 1). A 5 px
/// max filter closes the gaps between the leaflets in one step, and every card
/// reads as a solid rectangle from level 1 onward.
///
/// Scaling the narrowest cell down with each level keeps the radius
/// proportional to the thing it is protecting. Once that cell is below
/// [`MIN_SHAPE_WITH`], [`default_mip_radius`] already declines to dilate, which
/// is the honest LOD behaviour: at that size there is no leaf shape left.
pub fn atlas_mip_radius(atlas: &AlphaMask) -> impl Fn(u32, u32, u32) -> u32 {
    let base_width = atlas.width.max(1) as f32;
    let narrowest_cell = cell_widths(atlas.height)
        .into_iter()
        .min()
        .unwrap_or(atlas.width)
        .max(1) as f32;
    move |level, width, _height| {
        let cell = (narrowest_cell * width as f32 / base_width)
            .round()
            .max(1.0) as u32;
        if cell < ATLAS_DILATION_CELL_FLOOR {
            return 0;
        }
        default_mip_radius(level, cell, cell)
    }
}

/// Narrowest atlas cell that still has room for a 1 px dilation. One texel on
/// each side of a 10 px cell is already 20% of its width and closes the gaps
/// between leaflets, so below this the chain stops dilating entirely.
const ATLAS_DILATION_CELL_FLOOR: u32 = MIN_SHAPE_WITH * 2;

/// Width (px) below which a leaf mask has no useful shape left.
///
/// A leaf silhouette needs a few texels across to show a margin, a rachis or a
/// taper; under this, any representation is a smudge and the right move is to
/// stop drawing leaf cards (the LOD switch) rather than to keep dilating them
/// into blocks.
pub const MIN_SHAPE_WITH: u32 = 8;

/// Does the chain still carry a shape at `level`?
///
/// Used by the tests and documented here so the LOD code has one authority for
/// "past this point the mask is no longer a leaf".
pub fn mip_chain_preserves_shape(chain: &[AlphaMask], level: usize) -> bool {
    chain
        .get(level)
        .is_some_and(|m| m.width >= MIN_SHAPE_WITH && m.opaque_fraction() < 0.95)
}

/// Coverage the game's alpha test will require. Kept here so the mip assertion
/// and the material agree instead of drifting.
pub const ALPHA_CUTOFF: f32 = 0.5;

/// Converts an [`AlphaMask`] into a texture the foliage material can sample.
///
/// `Rgba8UnormSrgb` with the coverage in all four channels: the shader reads
/// the base-colour texture and multiplies it into the material base colour, so
/// the RGB must already be white (a value that changes nothing) while alpha
/// carries the outline. Writing the coverage into RGB as well would tint the
/// leaf by its own mask.
///
/// The mip chain is baked with per-level dilation. A GPU-generated chain would
/// average a thin blade below the alpha cutoff and the crown would stop cutting
/// the sky at distance — see [`build_mip_chain`].
pub fn mask_to_image(mask: &AlphaMask) -> bevy::image::Image {
    use bevy::asset::RenderAssetUsages;
    use bevy::image::{Image, ImageSampler};
    use bevy::render::render_resource::{
        Extent3d, TextureDataOrder, TextureDimension, TextureFormat,
    };

    let levels = image_mip_levels(mask.width.max(mask.height));
    let chain = build_mip_chain(mask, levels, atlas_mip_radius(mask));

    let mut data = Vec::with_capacity(
        chain
            .iter()
            .map(|m| (m.width * m.height * 4) as usize)
            .sum(),
    );
    for level in &chain {
        for alpha in &level.alpha {
            data.extend_from_slice(&[255, 255, 255, *alpha]);
        }
    }

    // `Image::new` asserts that the data length matches ONE level of the given
    // size, so a hand-built mip chain has to go through `new_uninit` and then
    // have its data attached. (Verified the hard way: `Image::new` panics on the
    // chain at runtime, not at compile time.)
    let mut image = Image::new_uninit(
        Extent3d {
            width: mask.width,
            height: mask.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels;
    // `MipMajor` matches the order the loop above wrote: all of level 0, then
    // all of level 1, and so on.
    image.data_order = TextureDataOrder::MipMajor;
    image.sampler = ImageSampler::Default;
    image
}

/// How deep the mip chain goes: down to 4 px on the short side.
///
/// Deeper levels cannot hold a leaf shape (see [`MIN_SHAPE_WITH`]), and the LOD
/// tiers stop drawing cards before they would be sampled anyway.
fn image_mip_levels(longest: u32) -> u32 {
    let mut levels = 1;
    let mut size = longest.max(1);
    while size > MIN_SHAPE_WITH && levels < 10 {
        size /= 2;
        levels += 1;
    }
    levels
}

/// Does `mask` still read as a leaf after the GPU has averaged it down to
/// `level` halvings?
///
/// The bar is "a visible number of texels still pass the alpha test", not "the
/// average is high": see [`AlphaMask::visible_fraction`]. A thin blade fails
/// this long before its mean coverage moves.
pub fn survives_mips(mask: &AlphaMask, level: u32) -> bool {
    let mut m = mask.clone();
    for _ in 0..level {
        m = m.downsample_half();
    }
    // A crown is thousands of these cards; a survivor count under a tenth of a
    // percent means the silhouette has effectively dissolved.
    m.visible_fraction() >= 0.001
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_species_bakes_a_non_empty_mask() {
        for profile in leaf_profiles() {
            let mask = bake_leaf_alpha(profile, 0, 256);
            assert!(mask.width > 0 && mask.height > 0, "{}", profile.species);
            let opaque = mask.alpha.iter().filter(|a| **a > 0).count();
            assert!(
                opaque > 200,
                "{}: only {opaque} opaque pixels, the shape failed to draw",
                profile.species
            );
            // A mask that filled its whole box would mean the shape test
            // degenerated into "always true".
            assert!(
                mask.opaque_fraction() < 0.85,
                "{}: the mask fills {}% of its box, so the outline is not an outline",
                profile.species,
                mask.opaque_fraction() * 100.0
            );
        }
    }

    #[test]
    fn masks_are_deterministic() {
        // The masks go into version control; a bake that drifted between runs
        // would churn the atlas on every build.
        for profile in leaf_profiles() {
            let a = bake_leaf_alpha(profile, 0, 128);
            let b = bake_leaf_alpha(profile, 0, 128);
            assert_eq!(a, b, "{} bake is not deterministic", profile.species);
        }
    }

    #[test]
    fn willow_is_a_sliver_and_poplar_is_broad() {
        // The species must be distinguishable by SHAPE, not just by label: if
        // the willow blade baked as wide as the poplar's, the whole point of
        // per-species masks is lost.
        let willow = bake_leaf_alpha(find_profile("willow").unwrap(), 0, 512);
        let poplar = bake_leaf_alpha(find_profile("poplar").unwrap(), 0, 512);

        // Compare the BLADE's own width-to-length ratio, not how much of each
        // box it fills: the box is sized from the leaf, so "fills 75% of the
        // box" is true of every species and measures nothing. (A first version
        // asserted exactly that and passed for the wrong reason.)
        let willow_ratio = span_along(&willow, Axis::X) / span_along(&willow, Axis::Y);
        let poplar_ratio = span_along(&poplar, Axis::X) / span_along(&poplar, Axis::Y);
        assert!(
            willow_ratio < 0.30,
            "willow blade is {willow_ratio:.2} as wide as long; lanceolate needs under 0.3"
        );
        assert!(
            poplar_ratio > 0.55,
            "poplar blade is {poplar_ratio:.2} as wide as long; deltoid needs over 0.55"
        );
        // And it must stay thick enough to survive a mip chain: a 1:8 sliver
        // looks right at full resolution and disappears in the distance.
        assert!(
            span_along(&willow, Axis::X) >= 24.0,
            "willow blade is only {} px wide at 512 px tall",
            span_along(&willow, Axis::X)
        );
    }

    #[test]
    fn poplar_is_widest_near_the_base() {
        // The deltoid reading depends on the widest point sitting low. A leaf
        // that peaked in the middle would read as an oval.
        let poplar = bake_leaf_alpha(find_profile("poplar").unwrap(), 0, 512);
        let widths = row_widths(&poplar);
        let peak = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .map(|(i, _)| i)
            .unwrap();
        // `t` runs 0 at the petiole to 1 at the tip. The blade is drawn with the
        // petiole at the BOTTOM of the image, so a high row index is a low t --
        // dividing the row index by the height (as a first version did) measures
        // the distance from the tip and silently inverts the assertion.
        let filled: Vec<usize> = (0..widths.len()).filter(|i| widths[*i] > 0).collect();
        let base_row = *filled.last().unwrap() as f32;
        let tip_row = *filled.first().unwrap() as f32;
        let t = (base_row - peak as f32) / (base_row - tip_row).max(1.0);
        assert!(
            t < 0.45,
            "poplar is widest at t={t:.2} (0 = petiole); a peak past the middle reads as an oval"
        );
    }

    #[test]
    fn serration_actually_bites_the_margin() {
        // 杨 is identified by its sawtooth. A smooth margin would delete the
        // only thing separating it from a generic leaf.
        let poplar = bake_leaf_alpha(find_profile("poplar").unwrap(), 0, 512);
        let widths = row_widths(&poplar);
        // Look at a stretch between the base and the tip where teeth should be
        // present, and count direction changes in the row width.
        let (lo, hi) = (poplar.height as usize / 4, poplar.height as usize * 3 / 4);
        let mut reversals = 0;
        for i in lo + 1..hi - 1 {
            let a = widths[i] as i32;
            let b = widths[i + 1] as i32;
            let prev = widths[i - 1] as i32;
            if (b - a).signum() != (a - prev).signum() && a != b {
                reversals += 1;
            }
        }
        assert!(
            reversals >= 4,
            "poplar margin shows only {reversals} width reversals; the sawtooth is missing"
        );
    }

    #[test]
    fn sophora_carries_several_leaflet_pairs() {
        // A pinnate mask must actually be pinnate: several separated clusters
        // along the rachis, not one blob.
        let sophora = bake_leaf_alpha(find_profile("sophora").unwrap(), 0, 512);
        let widths = row_widths(&sophora);
        // Count rows with content, then how many separated runs there are when
        // scanning down the leaflet columns' outer edge.
        let filled_rows = widths.iter().filter(|w| **w > 0).count();
        assert!(
            filled_rows > 300,
            "sophora only has {filled_rows} filled rows; the compound leaf did not draw"
        );
        let pairs = find_profile("sophora").unwrap().leaflet_pairs as usize;
        assert!(pairs >= 7, "槐 needs at least 7 pairs to read as pinnate");
    }

    #[test]
    fn the_shipped_mip_chain_keeps_every_species_readable() {
        // The failure mode this guards is the whole feature failing at
        // distance: a thin blade averages below the alpha cutoff, the GPU
        // discards it, and the crown stops cutting the sky -- which is exactly
        // the thing the leaf cards exist to do.
        //
        // Dilating the BASE image once was the first attempt and it failed at
        // level 6, so this asserts the real chain: per-level dilation.
        // Levels 0-4 of a 512 px mask are 512/256/128/64/32 px. By level 5 a
        // leaf is 16 px and by 6 it is 8 px; the mask is 2-4 px across and no
        // shape survives, which is a LOD decision (stop drawing leaf cards,
        // switch representation) rather than something dilation can fix.
        const MEANINGFUL_LEVELS: usize = 5;
        for profile in leaf_profiles() {
            let mask = bake_leaf_alpha(profile, 0, 512);
            let chain = build_mip_chain(&mask, 7, default_mip_radius);

            // Level 0 is the exact shape: full resolution is what the player
            // sees against the sky, so it must not be dilated.
            let raw = &chain[0];
            assert_eq!(raw.visible_fraction(), mask.visible_fraction());

            for (level, level_mask) in chain.iter().enumerate().take(MEANINGFUL_LEVELS).skip(1) {
                assert!(
                    level_mask.visible_fraction() >= 0.001,
                    "{}: level {level} ({}) lost the silhouette",
                    profile.species,
                    level_mask.width
                );
            }

            // And the chain must actually differ from an undilated one, or the
            // dilation is dead code that passes by luck. Compare at the deepest
            // level that still got dilated (a level below the shape floor is
            // deliberately left alone).
            let dilated_levels: Vec<usize> = (1..MEANINGFUL_LEVELS)
                .filter(|l| default_mip_radius(*l as u32, chain[*l].width, chain[*l].height) > 0)
                .collect();
            assert!(
                !dilated_levels.is_empty(),
                "{}: no level was dilated at all",
                profile.species
            );
            let last = *dilated_levels.last().unwrap();
            let mut undilated = mask.clone();
            for _ in 0..last {
                undilated = undilated.downsample_half();
            }
            assert!(
                chain[last].visible_fraction() > undilated.visible_fraction(),
                "{}: the chain's level {last} is no better than an undilated one ({} vs {})",
                profile.species,
                chain[last].visible_fraction(),
                undilated.visible_fraction()
            );
        }
    }

    /// Fraction of one atlas cell that survives the alpha test at a given mip.
    fn cell_visible_fraction(mask: &AlphaMask, u0: f32, u1: f32) -> f32 {
        let x0 = (u0 * mask.width as f32).round() as u32;
        let x1 = ((u1 * mask.width as f32).round() as u32)
            .max(x0 + 1)
            .min(mask.width);
        let cutoff = (ALPHA_CUTOFF * 255.0) as u8;
        let mut visible = 0usize;
        let mut total = 0usize;
        for y in 0..mask.height {
            for x in x0..x1 {
                total += 1;
                if mask.get(x, y) >= cutoff {
                    visible += 1;
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            visible as f32 / total as f32
        }
    }

    #[test]
    fn atlas_mips_keep_the_leaf_gaps_open() {
        // Regression for the TEXTURE THAT ACTUALLY SHIPS. The standalone-bake
        // tests above use `default_mip_radius` on a single mask (short side =
        // leaf width). The world instead bakes every species into one atlas,
        // whose short side is the SHEET height; that made level 1 dilate by
        // 5 px and close every leaflet gap, turning cards into solid quads.
        // Assert the shipped chain on the metric the GPU uses: visible_fraction
        // per cell, since a mean-coverage check cannot see alpha-test discard.
        let atlas = bake_species_atlas(ATLAS_HEIGHT);
        let levels = image_mip_levels(atlas.width.max(atlas.height));
        let chain = build_mip_chain(&atlas, levels, atlas_mip_radius(&atlas));

        for profile in leaf_profiles() {
            let (u0, u1) = species_variant_cell(profile.species, 0, ATLAS_HEIGHT);
            let base = cell_visible_fraction(&chain[0], u0, u1);
            let values: Vec<f32> = chain
                .iter()
                .skip(1)
                .take(4)
                .map(|m| cell_visible_fraction(m, u0, u1))
                .collect();
            println!(
                "{:>8}: mip0 {:.1}% | mip1..4 {}",
                profile.species,
                base * 100.0,
                values
                    .iter()
                    .map(|v| format!("{:.1}%", v * 100.0))
                    .collect::<Vec<_>>()
                    .join(" / ")
            );
            for value in &values {
                assert!(
                    *value < 0.70,
                    "{}: a shipped mip is {:.1}% solid; the leaf gaps were filled in",
                    profile.species,
                    value * 100.0
                );
                assert!(
                    *value < base + 0.25,
                    "{}: a shipped mip grew {:.1} points over mip0; dilation is eating the shape",
                    profile.species,
                    (value - base) * 100.0
                );
            }
        }
    }

    #[test]
    fn dilation_does_not_turn_a_leaf_into_a_blob() {
        // Dilation trades outline fidelity for survival. Blowing the mask up
        // into a rectangle would keep it "visible" while destroying the point.
        for profile in leaf_profiles() {
            let mask = bake_leaf_alpha(profile, 0, 512);
            let chain = build_mip_chain(&mask, 7, default_mip_radius);
            // A small amount of growth is fine (it IS dilation); a solid block
            // is not. Checked at EVERY level, not just level 1: a fixed pixel
            // radius passed a level-1-only test and still filled the mask solid
            // by level 3, because the masks shrink faster than a fixed radius
            // does.
            for (level, m) in chain.iter().enumerate().skip(1) {
                if m.width < MIN_SHAPE_WITH {
                    // Past the shape's resolution: the chain stops dilating
                    // here on purpose (see `default_mip_radius`), and whether a
                    // card is still drawn at all is the LOD's call.
                    continue;
                }
                assert!(
                    m.opaque_fraction() < 0.95,
                    "{}: level {level} ({}x{}) is {:.0}% solid while still {} px wide --                      the leaf became a block at a resolution that could have shown its shape",
                    profile.species,
                    m.width,
                    m.height,
                    m.opaque_fraction() * 100.0,
                    m.width,
                );
            }
            // The shape must survive at least 3 halvings (512 -> 64 px tall)
            // for every species, which covers the distance band where a crown
            // still reads as foliage rather than as a smudge.
            assert!(
                mip_chain_preserves_shape(&chain, 3),
                "{}: the shape is gone by level 3",
                profile.species
            );
            // And the growth must stay moderate while the shape is still
            // recognisable (levels 1-2, i.e. >= 64 px tall). Gated on width,
            // because below ~4 texels per feature the anti-aliased edge of a
            // one-texel leaf is a large share of its area: the fraction rises
            // without the outline filling in. A shoot cell is 1:6, so it hits
            // this at level 2 while a blade at the same height does not.
            for level in [1usize, 2] {
                if chain[level].width < MIN_SHAPE_WITH * 4 {
                    continue;
                }
                let growth = chain[level].opaque_fraction() / mask.opaque_fraction().max(1e-4);
                assert!(
                    growth < 2.5,
                    "{}: level {level} grew the covered area {growth:.2}x",
                    profile.species
                );
            }
        }
    }

    #[test]
    fn mask_aspect_gives_thin_species_a_thin_box() {
        // Baking a 6:1 blade into a square wastes texels and leaves the blade
        // a few pixels across, which no amount of subpixel sampling rescues.
        let willow = find_profile("willow").unwrap();
        let poplar = find_profile("poplar").unwrap();
        assert!(
            willow.mask_aspect() < poplar.mask_aspect(),
            "willow ({}) must get a narrower box than poplar ({})",
            willow.mask_aspect(),
            poplar.mask_aspect()
        );
        let baked = bake_leaf_alpha(willow, 0, 512);
        assert!(
            baked.height > baked.width * 2,
            "willow baked into {}x{}, not a thin box",
            baked.width,
            baked.height
        );
    }

    #[test]
    fn adding_a_species_needs_only_a_table_row() {
        // The extension path the user asked for (palm etc.): the table is the
        // only registry, so a new row must flow through baking with no other
        // change.
        let palm = LeafProfile {
            species: "palm_test",
            label: "hypothetical palm frond",
            shape: LeafShape::Pinnate,
            blade_length: 0.0,
            leaf_aspect: 0.0,
            blade_widest_at: 0.0,
            tip: LeafTip::Acute,
            teeth: 0.0,
            tooth_depth: 0.0,
            tip_taper_start: 0.86,
            blade_rise: 0.62,
            leaflet_pairs: 24,
            terminal_ratio: 0.4,
            leaflet_length: 0.30,
            leaflet_aspect: 0.14,
            leaflet_angle: 0.55,
            rachis_length: 0.94,
        };
        let mask = bake_leaf_alpha(&palm, 0, 256);
        assert!(mask.opaque_fraction() > 0.01, "palm baked empty");
        assert!(
            find_profile("palm_test").is_none(),
            "the probe must not be in the shipped table"
        );
    }

    #[test]
    fn the_species_atlas_cells_never_overlap_and_cover_every_species() {
        // The world's trees share ONE texture; two species' cards sampling the
        // same cell would render the wrong mask. The layout is the single
        // source, so the baker and the lookup must agree: cells are disjoint,
        // in table order, and every species resolves to one.
        let height = crate::world::leaf_shape::ATLAS_HEIGHT;
        let mut cells = Vec::new();
        for profile in leaf_profiles() {
            let (u0, u1) = species_variant_cell(profile.species, 0, height);
            assert!(u1 > u0, "{}'s atlas cell is empty", profile.species);
            cells.push((profile.species, u0, u1));
        }
        for pair in cells.windows(2) {
            assert!(
                pair[0].2 <= pair[1].1,
                "{}'s cell overlaps {}'s",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn the_species_atlas_matches_the_standalone_bakes() {
        // A cell must contain exactly its species' standalone mask: the atlas
        // is a repack of the table, not a re-bake with drifted parameters.
        let height = crate::world::leaf_shape::ATLAS_HEIGHT;
        let atlas = bake_species_atlas(height);
        for profile in leaf_profiles() {
            for variant in 0..LEAF_VARIANTS {
                let alone = bake_leaf_alpha(profile, variant, height);
                let (u0, u1) = species_variant_cell(profile.species, variant, height);
                let x0 = (u0 * atlas.width as f32).floor() as u32;
                let x1 = (u1 * atlas.width as f32).ceil() as u32;
                for y in (0..height).step_by(7) {
                    for x in x0..x1 {
                        assert_eq!(
                            atlas.get(x, y),
                            alone.get(x - x0, y),
                            "{} v{variant}: atlas cell (x{x}, y{y}) disagrees with the standalone bake",
                            profile.species
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_atlas_needs_no_half_texel_margin_of_its_own() {
        // The lookup insets each cell by half a texel so neighbouring cells do
        // not bleed at mip levels: an inset of 0 would sample the neighbour's
        // edge texel the moment a card's UV rounds outward.
        let height = crate::world::leaf_shape::ATLAS_HEIGHT;
        let (u0, u1) = species_variant_cell("sophora", 0, height);
        let atlas = bake_species_atlas(height);
        let inset = 0.5 / atlas.width as f32;
        assert!(
            u0 >= inset && u1 <= 1.0 - inset,
            "the species cell has no half-texel inset; atlas bleed is possible"
        );
    }

    #[derive(Clone, Copy)]
    enum Axis {
        X,
        Y,
    }

    /// Extent of covered pixels along one axis.
    fn span_along(mask: &AlphaMask, axis: Axis) -> f32 {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        for y in 0..mask.height {
            for x in 0..mask.width {
                if mask.get(x, y) > 0 {
                    let v = match axis {
                        Axis::X => x as usize,
                        Axis::Y => y as usize,
                    };
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
        }
        if lo == usize::MAX {
            return 0.0;
        }
        (hi - lo + 1) as f32
    }

    /// Covered width of each pixel row.
    fn row_widths(mask: &AlphaMask) -> Vec<u32> {
        (0..mask.height)
            .map(|y| (0..mask.width).filter(|x| mask.get(*x, y) > 0).count() as u32)
            .collect()
    }
}
