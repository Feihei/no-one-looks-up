//! Quality mapping, noise tiling and the per-kind shape table (pure data,
//! CPU-testable).

use bevy::math::Vec4;
use bevy_elements::ElementsQuality;

use crate::plugins::sky::CloudKind;

/// World size of the base noise tile, m.
///
/// 256 texels across it is 8 m per texel (the vendored layer's base volume is
/// 32 m per texel over 4096 m), so the billows it carries reach ~16–32 m
/// instead of 64–128 m — the direct lever on the blocky silhouette.
pub const BASE_TILE_M: f32 = 2048.0;
/// World size of the detail noise tile, m. 64 texels is 8 m per texel; its
/// finest Worley octave (32 cells) is 16 m across.
pub const DETAIL_TILE_M: f32 = 512.0;
/// Horizontal lean of cloud tops per unit of height fraction, m (wind shear).
pub const WIND_SKEW_M: f32 = 500.0;
/// Side of the weather map's world window, m.
///
/// The camera looks up 22.55–60° into a shell at 0.8–3.2 km, so all visible
/// cloud is within ~10 km; 16 km covers it. The map's field is periodic over
/// this window, so its repeat has no seam and its distribution does not depend
/// on where the window sits.
pub const WEATHER_EXTENT_M: f32 = 16_000.0;
/// Texels per side of the weather map (62.5 m per texel).
pub const WEATHER_TEXELS: u32 = 256;
/// Texels per side of the base noise volume.
pub const NOISE_BASE_TEXELS: u32 = 256;
/// Texels per side of the detail noise volume.
pub const NOISE_DETAIL_TEXELS: u32 = 64;
/// Frames the base volume is produced in, one z slab each.
pub const NOISE_PASSES: u32 = 8;
/// Slices of the base volume in one slab.
pub const NOISE_SLAB: u32 = NOISE_BASE_TEXELS / NOISE_PASSES;

/// Cloud rendering cost per [`ElementsQuality`] (R6; adr-0007). The march is
/// always half resolution per axis; the quality changes the step count and the
/// light-sample count only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudsQuality {
    Low,
    Medium,
    High,
    Ultra,
}

impl CloudsQuality {
    /// The settings of a quality level.
    pub fn of(quality: ElementsQuality) -> Self {
        match quality {
            ElementsQuality::Low => Self::Low,
            ElementsQuality::Medium => Self::Medium,
            ElementsQuality::High => Self::High,
            ElementsQuality::Ultra => Self::Ultra,
        }
    }

    /// Primary ray-march steps (every texel is marched every frame, so these
    /// are per-texel budgets, not amortised ones).
    pub fn steps(self) -> u32 {
        match self {
            Self::Low => 40,
            Self::Medium => 56,
            Self::High => 72,
            Self::Ultra => 96,
        }
    }

    /// Samples along the path to the light.
    pub fn light_samples(self) -> u32 {
        match self {
            Self::Low => 4,
            Self::Medium => 6,
            Self::High => 6,
            Self::Ultra => 8,
        }
    }

    /// Weight of the detail volume in the density, 0–1.
    ///
    /// `Low` drops the detail tap (one fetch per dense sample) and its wispy
    /// erosion: the base volume's billows still carry the silhouette, only the
    /// frayed edge goes. That is the biggest per-sample saving there is, and it
    /// is exactly the kind of detail a low-tier machine would lose to filtering
    /// anyway.
    pub fn detail_gain(self) -> f32 {
        match self {
            Self::Low => 0.0,
            Self::Medium | Self::High | Self::Ultra => 1.0,
        }
    }
}

/// The shape of one cloud kind, in the single cumulus model's parameter space.
///
/// Every kind is cumulus at heart; a deck is a cumulus with a flat gradient and
/// almost no dome shaping, not a second renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudShape {
    /// Height gradient quadruple (a0, a1, a2, a3): density ramps in between a0
    /// and a1 and out between a2 and a3, as fractions of the layer height.
    pub gradient: [f32; 4],
    /// Peak extinction at full density, m⁻¹.
    pub sigma: f32,
    /// Weight of the dome shaping, 0 (flat deck) to 1 (cumulus). It scales how
    /// much a blob's height follows its coverage: full in the core, shorter at
    /// the edge, which is what rounds a pile off instead of leaving a prism.
    pub dome: f32,
    /// Strength of the cloud-side detail erosion, 0–1.
    pub erosion: f32,
}

impl CloudShape {
    /// The parameters of `kind`.
    ///
    /// The cumulus kinds share the vendored layer's tuned quadruple
    /// `(0, 0.1625, 0.88, 0.98)`; the decks flatten it and drop the dome, so
    /// five kinds come out of one model.
    pub fn of(kind: CloudKind) -> Self {
        match kind {
            CloudKind::ScatteredCumulus => Self {
                gradient: [0.0, 0.1625, 0.88, 0.98],
                sigma: 0.085,
                dome: 1.0,
                erosion: 0.20,
            },
            CloudKind::BrokenCumulus => Self {
                gradient: [0.0, 0.1625, 0.88, 0.98],
                sigma: 0.10,
                dome: 1.0,
                erosion: 0.22,
            },
            CloudKind::ToweringCumulus => Self {
                gradient: [0.0, 0.10, 0.90, 1.0],
                sigma: 0.12,
                dome: 0.92,
                erosion: 0.18,
            },
            CloudKind::Stratocumulus => Self {
                gradient: [0.0, 0.10, 0.55, 0.72],
                sigma: 0.06,
                dome: 0.25,
                erosion: 0.28,
            },
            // A 300 m lid. Extinction is kept low: the sun sits at ~2.6°, so a
            // light path across the deck is many kilometres and a thicker one
            // goes black (C2's multiple scattering lifts the base).
            CloudKind::Overcast => Self {
                gradient: [0.0, 0.05, 0.85, 1.0],
                sigma: 0.04,
                dome: 0.0,
                erosion: 0.30,
            },
        }
    }

    /// The gradient quadruple as a uniform.
    pub fn gradient_vec(&self) -> Vec4 {
        Vec4::from_array(self.gradient)
    }
}

/// Wraps a drifted offset onto its tile period, m.
///
/// The cloud field is a periodic function of the world position, so drifting it
/// by whole periods changes nothing; wrapping keeps the shader's f32 precision
/// (and makes the drift bit-exactly reproducible in a test) however long the
/// session runs.
pub fn wrap_drift(drift: f64, period: f64) -> f32 {
    debug_assert!(period > 0.0, "a drift period must be positive");
    drift.rem_euclid(period) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_orders_step_counts() {
        let steps = |q| CloudsQuality::of(q).steps();
        assert!(steps(ElementsQuality::Low) <= steps(ElementsQuality::Medium));
        assert!(steps(ElementsQuality::Medium) <= steps(ElementsQuality::High));
        assert!(steps(ElementsQuality::High) <= steps(ElementsQuality::Ultra));
    }

    /// Only the low tier may drop the detail volume: the others would lose the
    /// frayed silhouette the whole rework is about.
    #[test]
    fn only_the_low_tier_drops_the_detail_volume() {
        assert_eq!(CloudsQuality::Low.detail_gain(), 0.0);
        for quality in [
            ElementsQuality::Medium,
            ElementsQuality::High,
            ElementsQuality::Ultra,
        ] {
            assert_eq!(CloudsQuality::of(quality).detail_gain(), 1.0);
        }
    }

    #[test]
    fn every_kind_has_a_valid_gradient() {
        for kind in CloudKind::ALL {
            let g = CloudShape::of(kind).gradient;
            assert!(
                g[0] < g[1] && g[1] <= g[2] && g[2] < g[3],
                "{kind:?} has an out-of-order gradient quadruple {g:?}"
            );
            assert!(
                (0.0..=1.0).contains(&g[0])
                    && (0.0..=1.0).contains(&g[1])
                    && (0.0..=1.0).contains(&g[2])
                    && (0.0..=1.0).contains(&g[3]),
                "{kind:?} has a gradient quadruple outside 0..1: {g:?}"
            );
        }
    }

    #[test]
    fn denser_kinds_are_more_extinct_and_decks_are_flatter() {
        // Cumulus kinds thicken with cover; the decks are thinner than any
        // cumulus and carry almost no dome shaping.
        assert!(
            CloudShape::of(CloudKind::ScatteredCumulus).sigma
                < CloudShape::of(CloudKind::BrokenCumulus).sigma
        );
        assert!(
            CloudShape::of(CloudKind::BrokenCumulus).sigma
                < CloudShape::of(CloudKind::ToweringCumulus).sigma
        );
        assert!(
            CloudShape::of(CloudKind::Overcast).sigma
                < CloudShape::of(CloudKind::Stratocumulus).sigma
        );
        assert_eq!(CloudShape::of(CloudKind::Overcast).dome, 0.0);
        assert!(CloudShape::of(CloudKind::Stratocumulus).dome < 0.5);
        for kind in [
            CloudKind::ScatteredCumulus,
            CloudKind::BrokenCumulus,
            CloudKind::ToweringCumulus,
        ] {
            assert!(CloudShape::of(kind).dome >= 0.9, "{kind:?} lost its dome");
        }
    }

    #[test]
    fn drifting_wraps_bit_exactly() {
        let period = BASE_TILE_M as f64;
        assert_eq!(wrap_drift(5.0, period), 5.0);
        // Whole periods vanish: the field is periodic, so this is invisible.
        assert_eq!(wrap_drift(period * 3.0 + 5.0, period), 5.0);
        // Negative drift still lands on the period, not at 0.
        assert_eq!(wrap_drift(-1.0, period), (period - 1.0) as f32);
        // A long session does not lose precision to a huge offset.
        assert_eq!(wrap_drift(1.0e6, period), (1.0e6 % period) as f32);
    }

    #[test]
    fn noise_tiling_is_square_and_slabbed_evenly() {
        assert_eq!(NOISE_BASE_TEXELS % NOISE_PASSES, 0);
        assert_eq!(NOISE_SLAB * NOISE_PASSES, NOISE_BASE_TEXELS);
        assert_eq!(NOISE_DETAIL_TEXELS % 4, 0);
        // Both volumes resolve the same world spacing, which is what keeps the
        // detail octaves from stepping against the base ones.
        let base_spacing = BASE_TILE_M / NOISE_BASE_TEXELS as f32;
        let detail_spacing = DETAIL_TILE_M / NOISE_DETAIL_TEXELS as f32;
        assert_eq!(base_spacing, detail_spacing);
    }
}
