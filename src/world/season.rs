//! Season parameter table (ADR-0004 §6, R3.5).
//!
//! Pure data on purpose, like [`crate::world::placement`]: a season resolves to
//! four numbers (leaf density, foliage colour, sway strength, sway speed) and
//! nothing here touches the ECS or the GPU, so the table can be asserted
//! headless.
//!
//! # Why the density is a geometry collapse, not an alpha cut
//!
//! The original Godot project shipped alpha-carded foliage: `ALPHA = texel.a *
//! leaf_density`, so winter simply made the cards transparent. The Bevy trees
//! are opaque baked meshes ([`crate::world::tree_mesh`]) with no texture and no
//! alpha channel, so that path is gone.
//!
//! Drawing the foliage with alpha would also be wrong for a second, subtler
//! reason: Bevy's depth prepass decides alpha discard from the *material* base
//! colour (`render/pbr_prepass_functions.wesl`,
//! `prepass_sample_color_and_alpha_discard`), never from a vertex colour. The
//! volumetric fog and the cloud pass both read that prepass depth. A per-vertex
//! alpha would therefore erase foliage in the colour pass while leaving it
//! solid in the depth buffer — visible as fog and clouds cut out around
//! invisible leaves.
//!
//! So `leaf_density` instead collapses each foliage vertex toward its own
//! cluster centre in the vertex shader. At 0 the clusters become points and the
//! bare branch skeleton is all that remains, which is exactly the winter look
//! ADR-0004 asks for — and the collapse happens before the vertex position is
//! written, so the main pass, the prepass and the shadow pass all agree.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// A season chosen when the walk starts (ADR-0001: fixed for the whole run, a
/// single run never changes season).
///
/// Serialized by variant name into [`crate::plugins::save_data::SaveData`], so
/// the spelling of these variants is part of the save format: renaming one
/// silently invalidates every existing save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Season {
    Spring,
    #[default]
    Summer,
    Autumn,
    Winter,
}

impl Season {
    /// All four, in calendar order (also the order the dev keys 1-4 use).
    pub const ALL: [Season; 4] = [
        Season::Spring,
        Season::Summer,
        Season::Autumn,
        Season::Winter,
    ];

    /// The parameters ADR-0004 §6 specifies for this season.
    pub fn params(self) -> SeasonParams {
        match self {
            Season::Spring => SeasonParams {
                leaf_density: 0.7,
                // Fresh new growth: yellow-green.
                leaf_color_srgb: [0.6, 0.8, 0.3],
                sway_strength: 0.03,
            },
            Season::Summer => SeasonParams {
                leaf_density: 1.0,
                // Dense deep green.
                leaf_color_srgb: [0.2, 0.5, 0.2],
                sway_strength: 0.03,
            },
            Season::Autumn => SeasonParams {
                leaf_density: 0.6,
                // Gold, falling.
                leaf_color_srgb: [0.8, 0.5, 0.1],
                sway_strength: 0.04,
            },
            Season::Winter => SeasonParams {
                // 0 = no foliage at all; the branch skeleton carries the tree.
                leaf_density: 0.0,
                // Bare grey-brown.
                leaf_color_srgb: [0.5, 0.45, 0.4],
                sway_strength: 0.05,
            },
        }
    }

    /// Short name for logs (English: logs stay ASCII so they grep cleanly).
    pub fn label(self) -> &'static str {
        match self {
            Season::Spring => "Spring",
            Season::Summer => "Summer",
            Season::Autumn => "Autumn",
            Season::Winter => "Winter",
        }
    }

    /// Short name for the player-facing menu (Chinese, like every other
    /// on-screen string).
    pub fn label_zh(self) -> &'static str {
        match self {
            Season::Spring => "春",
            Season::Summer => "夏",
            Season::Autumn => "秋",
            Season::Winter => "冬",
        }
    }

    /// The next season in calendar order, wrapping winter back to spring.
    pub fn next(self) -> Season {
        match self {
            Season::Spring => Season::Summer,
            Season::Summer => Season::Autumn,
            Season::Autumn => Season::Winter,
            Season::Winter => Season::Spring,
        }
    }

    /// The previous season in calendar order, wrapping spring back to winter
    /// (the menu's season editor steps in both directions).
    pub fn prev(self) -> Season {
        match self {
            Season::Spring => Season::Winter,
            Season::Summer => Season::Spring,
            Season::Autumn => Season::Summer,
            Season::Winter => Season::Autumn,
        }
    }
}

/// The four numbers a season resolves to (ADR-0004 §6).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeasonParams {
    /// 0 = bare branches, 1 = fully leafed.
    pub leaf_density: f32,
    /// Foliage hue, sRGB, as ADR-0004 §6 lists it.
    pub leaf_color_srgb: [f32; 3],
    /// Peak vertex displacement (m) at the crown's outer edge.
    pub sway_strength: f32,
}

impl SeasonParams {
    /// Foliage tint in **linear** space, normalised so its luminance is 1.0.
    ///
    /// This is a *multiplier*, not a colour: `StandardMaterial`'s base colour
    /// is already the near-black silhouette, and the PBR shader computes
    /// `base_color = vertex_color * material.base_color`. Feeding this as the
    /// vertex colour therefore swaps the crown's hue while leaving its value
    /// exactly where ADR-0001 wants it — a shape against the dusk sky, never a
    /// lit mid-green tree.
    ///
    /// Normalising to 1.0 (rather than to [`SILHOUETTE_LUMINANCE`]) is what
    /// makes the multiplication correct: the silhouette arrives through the
    /// material, so the tint only has to carry the hue.
    pub fn foliage_tint(self) -> Vec3 {
        let srgb = Vec3::from_array(self.leaf_color_srgb);
        let linear = Vec3::new(
            srgb_to_linear(srgb.x),
            srgb_to_linear(srgb.y),
            srgb_to_linear(srgb.z),
        );
        let luminance = 0.2126 * linear.x + 0.7152 * linear.y + 0.0722 * linear.z;
        if luminance <= 1e-6 {
            // A pure black season colour has no hue to preserve; a zero
            // multiplier would erase the crown instead of tinting it.
            return Vec3::ONE;
        }
        linear / luminance
    }

    /// The season's foliage colour at the shipped silhouette luminance.
    ///
    /// `foliage_tint() * SILHOUETTE_LUMINANCE`, spelled out because tests and
    /// ADR readers want the absolute colour, while the GPU needs the ratio.
    pub fn foliage_linear(self) -> Vec3 {
        self.foliage_tint() * SILHOUETTE_LUMINANCE
    }

    /// The `leaf_density` actually handed to the shader, clamped to the range
    /// the geometry collapse is valid for.
    pub fn shader_density(self) -> f32 {
        self.leaf_density.clamp(0.0, 1.0)
    }

    /// Sway amplitude in metres, clamped so a mistyped table cannot throw
    /// vertices across the street.
    pub fn shader_sway(self) -> f32 {
        self.sway_strength.clamp(0.0, 0.25)
    }
}

/// Luminance every foliage tint is normalised to, in **linear** space.
///
/// The shipped silhouette is `Color::srgb(0.030, 0.038, 0.032)`
/// (`tree_mesh::silhouette_material`); those are sRGB values, so the linear
/// luminance is ~0.0028, not 0.036. Getting this wrong by using the sRGB
/// numbers directly would make every tree ~17x brighter and turn the
/// silhouettes into lit foliage, which is the one look ADR-0001 rules out.
pub const SILHOUETTE_LUMINANCE: f32 = 0.0028;

/// How fast the sway oscillates (rad/s of the sine phase). Kept out of the
/// per-season table: ADR-0004 lists only density/colour/strength, and the
/// original shader's `sway_speed` was a material constant, not a season knob.
pub const SWAY_SPEED: f32 = 1.1;

/// Spatial frequency of the sway (1/m): how much the phase changes per metre of
/// tree, which is what makes neighbouring branches move out of step instead of
/// the whole crown sliding sideways.
pub const SWAY_PHASE_LEN: f32 = 0.9;

/// The season of the current run (ADR-0001: chosen at start, constant within a
/// run). Dev builds may cycle it with the number keys; a real run only reads
/// the value the menu or the save put here.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SeasonState {
    pub season: Season,
}

impl SeasonState {
    pub fn new(season: Season) -> Self {
        Self { season }
    }

    pub fn params(&self) -> SeasonParams {
        self.season.params()
    }
}

/// sRGB transfer function (the exact IEC 61966-2-1 curve, not a pow(2.2)
/// approximation): the table in ADR-0004 is in sRGB and the GPU wants linear.
fn srgb_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_matches_adr_0004() {
        // The four rows of ADR-0004 §6, asserted verbatim: this table is the
        // design, not an implementation detail, so a "tidy-up" that rounds a
        // value has to fail here first.
        let expected = [
            (Season::Spring, 0.7, [0.6, 0.8, 0.3], 0.03),
            (Season::Summer, 1.0, [0.2, 0.5, 0.2], 0.03),
            (Season::Autumn, 0.6, [0.8, 0.5, 0.1], 0.04),
            (Season::Winter, 0.0, [0.5, 0.45, 0.4], 0.05),
        ];
        for (season, density, color, sway) in expected {
            let params = season.params();
            assert_eq!(params.leaf_density, density, "{season:?} density");
            assert_eq!(params.leaf_color_srgb, color, "{season:?} colour");
            assert_eq!(params.sway_strength, sway, "{season:?} sway");
        }
    }

    #[test]
    fn winter_is_the_only_bare_season() {
        // ADR-0004: winter at 0 makes the branch skeleton the visible tree.
        // Anything else reaching 0 would make a leafy season look dead.
        for season in Season::ALL {
            let density = season.params().leaf_density;
            if season == Season::Winter {
                assert_eq!(density, 0.0, "winter must have no leaves");
            } else {
                assert!(
                    density > 0.5,
                    "{season:?} at {density} would read as a bare tree"
                );
            }
        }
    }

    #[test]
    fn every_season_stays_inside_the_silhouette_budget() {
        // The whole point of normalising luminance: a season swap must change
        // the crown's hue, never its value, or autumn turns into a bright
        // orange blob against the dusk sky (which is what ADR-0001 forbids).
        for season in Season::ALL {
            let c = season.params().foliage_linear();
            let luminance = 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
            assert!(
                (luminance - SILHOUETTE_LUMINANCE).abs() < 1e-5,
                "{season:?} foliage luminance {luminance} escapes the silhouette budget"
            );
            assert!(
                c.min_element() >= 0.0,
                "{season:?} produced a negative channel"
            );
        }
        // The budget itself must match the shipped material. `Color::srgb`
        // values are not linear, and using them unconverted would make trees
        // ~17x too bright; assert the conversion rather than trusting it.
        let base = super::super::tree_mesh::SILHOUETTE_BASE_COLOR_SRGB;
        let linear = Vec3::new(
            srgb_to_linear(base[0]),
            srgb_to_linear(base[1]),
            srgb_to_linear(base[2]),
        );
        let shipped = 0.2126 * linear.x + 0.7152 * linear.y + 0.0722 * linear.z;
        assert!(
            (shipped - SILHOUETTE_LUMINANCE).abs() < 1e-4,
            "SILHOUETTE_LUMINANCE {SILHOUETTE_LUMINANCE} does not match the shipped tree material ({shipped})"
        );
    }

    #[test]
    fn foliage_tint_is_a_hue_preserving_multiplier() {
        // The GPU multiplies the vertex colour into the material base colour,
        // so the tint must have unit luminance. A tint carrying the absolute
        // luminance instead would darken every tree twice.
        for season in Season::ALL {
            let tint = season.params().foliage_tint();
            let luminance = 0.2126 * tint.x + 0.7152 * tint.y + 0.0722 * tint.z;
            assert!(
                (luminance - 1.0).abs() < 1e-4,
                "{season:?} tint luminance {luminance} is not 1.0"
            );
        }
    }

    #[test]
    fn seasons_are_visually_distinct() {
        // Four identical tints would make the feature invisible while every
        // other test still passed.
        for pair in Season::ALL.windows(2) {
            let a = pair[0].params().foliage_linear();
            let b = pair[1].params().foliage_linear();
            assert!(
                (a - b).length() > 1e-3,
                "{:?} and {:?} share a foliage tint",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn shader_parameters_are_clamped() {
        // A table entry outside these ranges would either throw vertices off
        // the tree (sway) or extrapolate the collapse past the cluster centre.
        let wild = SeasonParams {
            leaf_density: 3.0,
            leaf_color_srgb: [2.0, -1.0, 0.5],
            sway_strength: 9.0,
        };
        assert_eq!(wild.shader_density(), 1.0);
        assert!(wild.shader_sway() <= 0.25);
        let negative = SeasonParams {
            leaf_density: -2.0,
            sway_strength: 0.0,
            ..wild
        };
        assert_eq!(negative.shader_density(), 0.0);

        // The real table is already inside the ranges, so the clamp is a guard
        // and not a silent correction.
        for season in Season::ALL {
            let p = season.params();
            assert_eq!(p.shader_density(), p.leaf_density);
            assert_eq!(p.shader_sway(), p.sway_strength);
        }
    }

    #[test]
    fn next_cycles_through_all_four_and_returns() {
        let mut season = Season::Spring;
        let mut seen = vec![season];
        for _ in 0..3 {
            season = season.next();
            seen.push(season);
        }
        assert_eq!(seen, Season::ALL.to_vec());
        assert_eq!(season.next(), Season::Spring, "winter must wrap to spring");
    }

    #[test]
    fn default_is_summer_and_matches_the_save_placeholder() {
        // R4.2 shipped a placeholder enum with only `Summer`, and saves on
        // disk say "Summer". The default has to stay put or those saves change
        // meaning.
        assert_eq!(Season::default(), Season::Summer);
        assert_eq!(
            serde_json::to_string(&Season::Summer).unwrap(),
            "\"Summer\""
        );
        // And every variant round-trips by name, so no save needs migrating.
        for season in Season::ALL {
            let json = serde_json::to_string(&season).unwrap();
            assert_eq!(serde_json::from_str::<Season>(&json).unwrap(), season);
        }
    }
}
