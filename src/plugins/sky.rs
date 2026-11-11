//! Sky system plugin (R1).
//!
//! Wraps bevy_elements behind a locked golden-hour sky: 17:30 local time,
//! paused clock, forced clear weather, Beijing-like observer location
//! (ADR-0001/0003). bevy_elements subsystems the game does not use (wind,
//! precipitation, lightning, dust, terrain, hydrology, water, ground cover,
//! weathering) are disabled at plugin-group level (R1.4).
//!
//! Ordering contract (root AGENTS.md: no implicit schedule dependence): every
//! system this plugin adds is explicitly ordered against
//! [`bevy_elements::ElementsSystems`].

use std::time::Duration;

use bevy::camera::Exposure;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;

use bevy_elements::{
    CivilDate, Climate, CloudPlugin, DustPlugin, ElementsCamera, ElementsPlugins, ElementsSystems,
    GeoLocation, GroundCoverPlugin, HydrologyPlugin, LightningPlugin, PathsPlugin,
    PrecipitationPlugin, RocksPlugin, TerrainPlugin, TrailsPlugin, WaterPlugin, WavesPlugin,
    WeatherDirector, WeatherPreset, WeatheredPlugin, WindPlugin, WorldClock,
};

/// A named low-cloud look.
///
/// bevy_elements renders ONE volumetric layer whose type blends between
/// stratus (flat sheets) and cumulus/cumulonimbus (vertical towers) from the
/// weather values. Each variant here is a tuned point in that space, so
/// switching kinds changes what the same renderer produces.
///
/// Measured constraint (see the module notes on granularity): the cloud
/// silhouette is quantised at ~32-64 m by the crate's fixed noise
/// resolutions, so these knobs change the *distribution and proportions* of
/// the shapes, not their edge detail. Frayed, irregular edges need a noise
/// change, which means patching the dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudKind {
    /// Fair-weather cumulus: distinct puffs with clear sky between them.
    ScatteredCumulus,
    /// Heavier cumulus: more cover, taller, still separated.
    BrokenCumulus,
    /// Cumulonimbus: towering, dense, most of the sky.
    ToweringCumulus,
    /// Stratocumulus: a broken sheet of low cloud, flat and wide.
    Stratocumulus,
    /// Overcast stratus: an unbroken grey lid.
    Overcast,
}

impl CloudKind {
    /// Display order for the panel and cycling.
    pub const ALL: [CloudKind; 5] = [
        CloudKind::ScatteredCumulus,
        CloudKind::BrokenCumulus,
        CloudKind::ToweringCumulus,
        CloudKind::Stratocumulus,
        CloudKind::Overcast,
    ];

    /// Short name for the panel row.
    pub fn label(self) -> &'static str {
        match self {
            CloudKind::ScatteredCumulus => "scattered cu",
            CloudKind::BrokenCumulus => "broken cu",
            CloudKind::ToweringCumulus => "towering cb",
            CloudKind::Stratocumulus => "stratocumulus",
            CloudKind::Overcast => "overcast",
        }
    }

    /// Cover at `amount = 1` for this kind, 0..1.
    pub fn base_cover(self) -> f32 {
        match self {
            CloudKind::ScatteredCumulus => 0.30,
            CloudKind::BrokenCumulus => 0.55,
            CloudKind::ToweringCumulus => 0.80,
            CloudKind::Stratocumulus => 0.78,
            // 0.9 rather than 0.97: the shader's deck mode saturates above
            // ~0.95, which is where the layer stops showing any structure.
            CloudKind::Overcast => 0.90,
        }
    }

    /// Relative humidity this kind wants, 0..1. Above ~0.9 the crate's
    /// `kind_from_weather` swings the cloud type to stratus, which is what
    /// turns cumulus into a sheet.
    pub fn humidity(self) -> f32 {
        match self {
            CloudKind::ScatteredCumulus => 0.45,
            CloudKind::BrokenCumulus => 0.55,
            CloudKind::ToweringCumulus => 0.70,
            CloudKind::Stratocumulus => 0.93,
            CloudKind::Overcast => 0.95,
        }
    }

    /// Storminess, 0..6. `storminess / 6` is the crate's cumulonimbus weight.
    pub fn storminess(self) -> f32 {
        match self {
            CloudKind::ScatteredCumulus => 0.0,
            CloudKind::BrokenCumulus => 0.3,
            CloudKind::ToweringCumulus => 2.2,
            CloudKind::Stratocumulus => 0.0,
            CloudKind::Overcast => 0.0,
        }
    }

    /// Cloud base and top (m). Stratus is a thin low lid; cumulonimbus is the
    /// tall one. `None` lets the crate derive the layer from the weather.
    pub fn layer(self) -> (f32, f32) {
        match self {
            CloudKind::ScatteredCumulus => (1200.0, 2600.0),
            CloudKind::BrokenCumulus => (1100.0, 3200.0),
            CloudKind::ToweringCumulus => (800.0, 7000.0),
            // Thin and wide is what makes a stratocumulus deck; it is also
            // the only way to keep the shader's dome factor weak (see the
            // CloudKind docs).
            CloudKind::Stratocumulus => (900.0, 1300.0),
            // A 300 m lid. A thick slab goes black at dusk: the sun sits at
            // ~2.4 deg, so light has to cross the whole layer at a grazing
            // angle to reach the base, and at 600 m it never arrives. Thin
            // overcast still reads as unbroken cover without the blackout.
            CloudKind::Overcast => (700.0, 1000.0),
        }
    }

    /// Wind speed (m/s). Above 12 the crate grows towers on its own, so the
    /// stormy kinds keep it high deliberately.
    pub fn wind(self) -> f32 {
        match self {
            CloudKind::ToweringCumulus => 9.0,
            CloudKind::BrokenCumulus => 5.0,
            _ => 3.0,
        }
    }
}

/// Golden-hour sky configuration (ADR-0001: fixed dusk, no weather changes).
///
/// Insert this resource before adding [`SkyPlugin`] to override the defaults.
#[derive(Resource, Clone, Debug)]
pub struct DuskSky {
    /// Calendar date driving both the sun altitude at the golden hour and the
    /// moon phase. The two are coupled through the date (a fixed location +
    /// fixed hour leaves the date as the single tuning knob).
    pub date: CivilDate,
    /// Locked local hour. The game freezes time here (golden hour, 17:30).
    pub golden_hour: f64,
    /// Observer location; also selects the time zone for the local hour.
    ///
    /// Longitude is tuned so the fixed local hour lands the sun mid-way
    /// through the dusk tint (≈5° altitude): the game's world is fictional,
    /// so the coordinates only need to produce the adr-0001 gradient.
    pub location: GeoLocation,
    /// Which kind of low cloud the sky carries.
    ///
    /// bevy_elements has exactly one volumetric layer, and its type is a
    /// blend between stratus and cumulus driven by weather. A "kind" here is
    /// therefore a named combination of cover / humidity / storminess /
    /// layer thickness, not a separate renderer. `None` means a clear sky.
    pub cloud_kind: Option<CloudKind>,
    /// How much of the selected kind, 0..1. Scales the kind's own cover.
    pub cloud_amount: f32,
    /// Overrides the derived `CloudLayer` altitudes (m). Applied with one
    /// frame of lag (see `apply_cloud_layer_override`).
    pub cloud_layer: Option<(f32, f32)>,
    /// Base photographic exposure (ev100). ADR-0005 maps the golden-hour look
    /// to a fixed `ev100 ≈ 13.0` with ACES tonemapping.
    pub exposure_ev100: f32,
    /// Peak amplitude of the world-position exposure perturbation (R1.5), in
    /// ev100 stops. Deterministic function of the camera position.
    pub exposure_noise_amplitude: f32,
    /// Extra humidity offset applied on top of the kind's own value (-1..1).
    ///
    /// The kind sets a sensible baseline; this lets a run push the same kind
    /// toward sheet-like or broken without switching kinds.
    pub humidity_bias: f32,
    /// Extra storminess offset (-1..1). Pushes the kind toward towering
    /// cumulonimbus without changing its cover.
    pub storminess_bias: f32,
    /// Wind speed at 10 m (m/s). Above ~12 m/s with heavy cover the clouds grow
    /// towers; it also sets how fast the field drifts.
    pub wind_speed_ms: f32,
    /// Bloom intensity (R1.3 tuned this by eye; exposed so it can be retuned
    /// without a rebuild).
    pub bloom_intensity: f32,
}

impl Default for DuskSky {
    fn default() -> Self {
        Self {
            // Equinox: sunset over Beijing is ~18:15, so 17:30 lands the sun
            // ~10° above the horizon — the `DUSK_ONSET_DEG = 10°` band where
            // bevy_elements blends toward its golden dusk tint.
            date: CivilDate::new(2026, 3, 21),
            golden_hour: 17.5,
            location: GeoLocation {
                latitude_deg: 39.9042,
                // Tuned so 17:30 lands the sun at ~2.6° altitude: dusk tint
                // factor ≈ 0.75 (deeper orange horizon) and a relatively
                // darker blue-purple zenith (Feihei: "slightly more
                // orange-purple").
                longitude_deg: 126.4,
                utc_offset_hours: 8.0,
            },
            // Volume clouds start OFF. Feihei's call (2026-10-27): the crate's
            // volumetric layer reads as blocky geometry at the golden hour and
            // no parameter fixes that (the silhouette is quantised at 32-64 m
            // by fixed noise resolutions). A clear dusk is also closer to
            // ADR-0001, and cirrus is available as a cheap alternative. Turn a
            // kind back on with F5 when it is wanted.
            cloud_kind: None,
            cloud_amount: 0.0,
            cloud_layer: None,
            exposure_ev100: 13.0,
            exposure_noise_amplitude: 0.15,
            humidity_bias: 0.0,
            storminess_bias: 0.0,
            wind_speed_ms: 3.0,
            bloom_intensity: 0.3,
        }
    }
}

pub struct SkyPlugin;

impl Plugin for SkyPlugin {
    fn build(&self, app: &mut App) {
        let sky = app
            .world()
            .get_resource::<DuskSky>()
            .cloned()
            .unwrap_or_default();
        app.insert_resource(sky.clone());

        // Frozen golden hour: time_scale 0.0 and paused are equivalent
        // (bevy_elements-notes.md §3); paused states the intent explicitly.
        let clock = WorldClock::new(sky.date)
            .at_hour(sky.golden_hour)
            .with_time_scale(0.0);
        let mut director = WeatherDirector::default();
        director.force(WeatherPreset::Clear, Duration::ZERO);

        app.insert_resource(clock)
            .insert_resource(Climate::temperate())
            .insert_resource(sky.location.clone())
            .insert_resource(director)
            .add_plugins(
                ElementsPlugins
                    .build()
                    // R1.4: the game has no weather changes — drop everything
                    // outside climate/celestial/fog (ADR-0001). 0.6.0
                    // added Trails/Paths/Waves/Rocks to the default group; the
                    // game uses none of them. R6: the crate's own volumetric
                    // layer is replaced by the game's CloudsPlugin (adr-0007);
                    // with CloudPlugin off the crate's celestial/fog estimate
                    // the cloud dimming from `Weather` (`1 − cover^3.4`),
                    // which apply_weather_overrides keeps in step.
                    .disable::<CloudPlugin>()
                    .disable::<WindPlugin>()
                    .disable::<PrecipitationPlugin>()
                    .disable::<LightningPlugin>()
                    .disable::<DustPlugin>()
                    .disable::<TrailsPlugin>()
                    .disable::<PathsPlugin>()
                    .disable::<WavesPlugin>()
                    .disable::<RocksPlugin>()
                    .disable::<TerrainPlugin>()
                    .disable::<HydrologyPlugin>()
                    .disable::<WaterPlugin>()
                    .disable::<GroundCoverPlugin>()
                    .disable::<WeatheredPlugin>(),
            )
            .add_systems(
                Update,
                (
                    // Runs between the weather director (set `Weather`) and
                    // the celestial/cloud consumers (set `Celestial`), so one
                    // coherent overridden `Weather` reaches lighting and the
                    // cloud layer derivation.
                    apply_weather_overrides
                        .after(ElementsSystems::Weather)
                        .before(ElementsSystems::Celestial),
                    perturb_exposure,
                    apply_bloom_intensity,
                ),
            );
    }
}

/// Applies the tunable weather knobs to the director's `Weather` every frame.
///
/// `cloud_cover: None` keeps the forced-clear preset value, but humidity /
/// storminess / wind are always applied: they only shape the cloud TYPE, and a
/// panel that silently ignores three of its four knobs would be a trap. Wind is
/// floored at the preset's value so a still day still drifts a little.
/// Resolves the selected cloud kind plus its amount into the concrete weather
/// the crate consumes.
///
/// Returns `None` for a clear sky (no kind), which is a real state the game
/// needs: ADR-0001's golden hour is meant to be mostly clear, and an
/// overcast-only renderer could not produce it.
pub fn resolve_clouds(sky: &DuskSky) -> Option<ResolvedClouds> {
    let kind = sky.cloud_kind?;
    let amount = sky.cloud_amount.clamp(0.0, 1.0);
    // Cover scales with amount; a slip of cover at amount 0 keeps the crate's
    // histogram from dividing by zero but shows nothing.
    let cover = if amount <= 0.001 {
        0.0
    } else {
        (kind.base_cover() * amount).clamp(0.0, 1.0)
    };
    Some(ResolvedClouds {
        cover,
        humidity: (kind.humidity() + sky.humidity_bias).clamp(0.0, 1.0),
        storminess: (kind.storminess() + sky.storminess_bias).clamp(0.0, 6.0),
        wind: kind.wind(),
        layer: sky.cloud_layer.unwrap_or_else(|| kind.layer()),
    })
}

/// The weather a [`CloudKind`] resolves to. Plain data so the mapping can be
/// asserted without a GPU.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedClouds {
    pub cover: f32,
    pub humidity: f32,
    pub storminess: f32,
    pub wind: f32,
    pub layer: (f32, f32),
}

fn apply_weather_overrides(sky: Res<DuskSky>, mut weather: ResMut<bevy_elements::Weather>) {
    match resolve_clouds(&sky) {
        Some(clouds) => {
            weather.cloud_cover = clouds.cover;
            weather.relative_humidity = clouds.humidity;
            weather.storminess = clouds.storminess;
            weather.wind_speed_ms = clouds.wind;
        }
        // Clear sky: no coverage at all, and humidity low enough that the
        // crate's own `kind_from_weather` cannot conjure a sheet from nothing.
        None => {
            weather.cloud_cover = 0.0;
            weather.relative_humidity = 0.35;
            weather.storminess = 0.0;
            weather.wind_speed_ms = 2.0;
        }
    }
}

/// R1.5: subtle exposure drift driven by a deterministic function of the
/// camera's world position (sum-of-sines noise; no RNG on the frame path).
///
/// Note: the atmosphere's `ScatteringMedium` is a GPU LUT asset — per-position
/// density changes would rebuild it every frame, so only the exposure half of
/// the ADR's "density multiplier / exposure jitter" is implemented (recorded
/// in bevy-elements-notes.md).
fn perturb_exposure(
    sky: Res<DuskSky>,
    cameras: Query<(&GlobalTransform, &mut Exposure), With<ElementsCamera>>,
) {
    let amplitude = sky.exposure_noise_amplitude;
    if amplitude == 0.0 {
        return;
    }
    for (transform, mut exposure) in cameras {
        let p = transform.translation();
        let noise = (p.x * 0.013).sin() * 0.5
            + (p.z * 0.021 + 1.7).sin() * 0.3
            + ((p.x + p.z) * 0.005 + 4.1).sin() * 0.2;
        exposure.ev100 = sky.exposure_ev100 + amplitude * noise;
    }
}

/// Keeps every `GoldenHourCamera` bloom in step with `DuskSky::bloom_intensity`,
/// so the panel's bloom knob takes effect on the next frame.
fn apply_bloom_intensity(sky: Res<DuskSky>, mut cameras: Query<&mut Bloom, With<ElementsCamera>>) {
    for mut bloom in &mut cameras {
        if bloom.intensity != sky.bloom_intensity {
            bloom.intensity = sky.bloom_intensity;
        }
    }
}

/// Camera component set for the golden-hour look (ADR-0005 §4): fixed
/// exposure, ACES tonemapping, natural bloom. Attach alongside
/// `ElementsCamera`.
#[derive(Bundle)]
pub struct GoldenHourCamera {
    pub elements_camera: ElementsCamera,
    pub exposure: Exposure,
    pub tonemapping: Tonemapping,
    pub bloom: Bloom,
}

impl Default for GoldenHourCamera {
    fn default() -> Self {
        Self {
            elements_camera: ElementsCamera {
                auto_exposure: false,
                ..ElementsCamera::default()
            },
            exposure: Exposure {
                ev100: DuskSky::default().exposure_ev100,
            },
            tonemapping: Tonemapping::AcesFitted,
            // Above NATURAL's 0.15: spreads the low-sun glow across the
            // horizon band (golden-hour look, R1.3 tuning).
            bloom: Bloom {
                intensity: 0.3,
                ..Bloom::NATURAL
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_clouds_ship_disabled() {
        // Feihei's decision (2026-10-27): the volumetric layer reads as blocky
        // geometry at the golden hour, and no parameter fixes it -- the
        // silhouette is quantised at 32-64 m by the crate's fixed noise
        // resolutions. The sky ships clear; F5 can turn a kind back on.
        //
        // Asserted because it is a deliberate art direction that a "sensible
        // default" refactor would happily undo.
        let sky = DuskSky::default();
        assert_eq!(
            sky.cloud_kind, None,
            "volume clouds must ship disabled; a default kind changes the look              of every existing save"
        );
        assert_eq!(sky.cloud_amount, 0.0);
        assert!(
            resolve_clouds(&sky).is_none(),
            "the default sky must resolve to no volumetric cloud"
        );
    }

    #[test]
    fn no_broken_or_overcast_layer_is_thick_enough_to_go_black() {
        // Measured: with a 600 m overcast lid the GPU's measured transmittance
        // under the camera decayed to ~1e-5 within ten seconds and the deck
        // rendered black. The sun sits at ~2.4 deg at the golden hour, so
        // light crosses the whole layer at a grazing angle and a thick,
        // UNBROKEN lid never lights its own base.
        //
        // Disconnected cumulus is exempt: light passes through the gaps
        // between puffs, and those kinds are the ones that already read
        // correctly. The constraint is on the sheet-like kinds, where every
        // ray meets the deck.
        for kind in CloudKind::ALL {
            let (base, top) = kind.layer();
            let thickness = top - base;
            // Feihei's field report: stratocumulus and overcast (the two
            // low-topped, high-cover kinds) went black, while the cumulus
            // kinds did not. Those two are the sheet-like ones, so the
            // constraint is scoped to them rather than to a coverage number:
            // a kind is sheet-like when its cover is high AND its top is low,
            // i.e. a lid rather than a puff.
            let sheet_like = kind.base_cover() >= 0.55 && top <= 2000.0;
            if sheet_like {
                assert!(
                    thickness <= 1000.0,
                    "{kind:?} covers {:.2} of the sky with a {thickness:.0} m lid; a                      sheet that thick goes black at the golden hour",
                    kind.base_cover()
                );
            }
            assert!(
                thickness >= 200.0,
                "{kind:?}: {thickness:.0} m is too thin to read"
            );
            assert!(
                kind.base_cover() <= 0.90,
                "{kind:?}: cover {} saturates the shader's deck mode",
                kind.base_cover()
            );
        }
    }

    /// Mirror of bevy_elements' `CloudLayer::kind_from_weather` (verified
    /// against the crate's source; it is `pub(crate)` there).
    fn kind_from_weather(cover: f32, rh: f32, storm: f32, wind: f32) -> f32 {
        fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
            let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        }
        let stratus = smoothstep(0.5, 0.9, cover).max(smoothstep(0.9, 0.98, rh));
        let towering = (storm / 6.0)
            .clamp(0.0, 1.0)
            .max(smoothstep(12.0, 16.0, wind) * smoothstep(0.9, 1.0, cover));
        0.5 * (1.0 - stratus) * (1.0 - towering) + towering
    }

    /// Mirror of the vendored crate's dome ramp (`common.wesl::cloud_gradient`,
    /// patched): `smoothstep(0.05, 0.55, kind)`. 0 = no dome, 1 = full
    /// cumulus rounding. Saturation now sits at `kind >= 0.55`; the sheet-like
    /// kinds keep well clear of it.
    fn dome_strength(kind: f32) -> f32 {
        fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
            let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        }
        smoothstep(0.05, 0.55, kind)
    }

    #[test]
    fn sheet_kinds_do_not_sit_in_the_dome_saturation_band() {
        // Feihei: "stratocumulus is all flat disc with half an egg on top,
        // only overcast looks natural". Cause: `dome_strength` is
        // `clamp(kind * 4, 0, 1)`, so it is fully saturated for any
        // `kind >= 0.25` — a stratocumulus at cover 0.65 / rh 0.86 produced
        // kind 0.342 and therefore the SAME dome as plain cumulus.
        //
        // The two sheet-like kinds must stay clear of that band. Cumulus is
        // exempt: there the dome is the point.
        for kind in [CloudKind::Stratocumulus, CloudKind::Overcast] {
            let (base, _) = kind.layer();
            let _ = base;
            let index = kind_from_weather(
                kind.base_cover(),
                kind.humidity(),
                kind.storminess(),
                kind.wind(),
            );
            assert!(
                index < 0.25,
                "{kind:?} resolves to cloud type {index:.3}, which saturates the                  shader's dome factor (>= 0.25) and renders as a disc with an egg                  on top; raise its cover instead"
            );
        }
    }

    #[test]
    fn cumulus_kinds_keep_the_dome() {
        // The flip side: if the cumulus kinds ever drop below the band the
        // puffs lose their rounded tops and read as slabs.
        for kind in [CloudKind::ScatteredCumulus, CloudKind::BrokenCumulus] {
            let index = kind_from_weather(
                kind.base_cover(),
                kind.humidity(),
                kind.storminess(),
                kind.wind(),
            );
            assert!(
                dome_strength(index) > 0.9,
                "{kind:?} lost its dome (cloud type {index:.3}); cumulus should                  stay rounded"
            );
        }
    }

    #[test]
    fn the_vendored_cloud_shader_patches_are_present() {
        // The vendored crate replaces upstream through `[patch.crates-io]`.
        // If it is dropped (a fresh `cargo update`, a forgotten re-merge) the
        // cloud artefacts come back silently, because nothing else in the
        // game depends on those three lines. This is the tripwire.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |rel: &str| {
            std::fs::read_to_string(root.join("vendor/bevy_elements").join(rel))
                .unwrap_or_else(|err| panic!("cannot read {rel}: {err}"))
        };

        let manifest = read("Cargo.toml");
        assert!(
            manifest.contains("name = \"bevy_elements\""),
            "the vendored crate is missing its own manifest"
        );
        let root_manifest =
            std::fs::read_to_string(root.join("Cargo.toml")).expect("root Cargo.toml");
        // Look for the actual assignment, not the substring: a commented-out
        // or renamed line still contains the path.
        let patched = root_manifest.lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#')
                && line.starts_with("bevy_elements")
                && line.contains("vendor/bevy_elements")
        });
        assert!(
            patched,
            "the root manifest no longer patches bevy_elements to the vendored copy;              the cloud fixes are not in effect"
        );

        // Dome: widened ramp, gentler profile, per-blob jitter.
        let common = read("src/clouds/common.wesl");
        assert!(
            common.contains("smoothstep(0.05, 0.55, kind)"),
            "the dome ramp is back to the saturating `clamp(kind * 4, 0, 1)`"
        );
        assert!(
            common.contains("pow(dome, 0.62)"),
            "the dome profile is back to the half-ellipse `sqrt(dome)`"
        );
        assert!(
            common.contains("dome_jitter"),
            "per-blob dome jitter is gone, so every cloud is identical again"
        );

        // Dome width.
        let weather = read("src/clouds/weather.wesl");
        assert!(
            weather.contains("const DOME_WIDTH: f32 = 0.30;"),
            "DOME_WIDTH is back to 0.37"
        );

        // Sky gain fade.
        let celestial = read("src/celestial/mod.rs");
        assert!(
            celestial.contains("dusk_gain_fade"),
            "the sky-gain dusk fade is gone; the sky will brighten as cloud rises"
        );
    }

    #[test]
    fn stormy_kinds_are_the_tall_ones() {
        // Towering cumulus is the one kind allowed to be tall: its height IS
        // the feature. It is also the one the crate's storminess heuristic
        // expects to tower.
        let tall = CloudKind::ToweringCumulus.layer();
        for kind in CloudKind::ALL {
            if kind == CloudKind::ToweringCumulus {
                continue;
            }
            let layer = kind.layer();
            assert!(
                layer.1 < tall.1,
                "{kind:?} tops at {} m, not below towering {tall:?}",
                layer.1
            );
        }
        assert!(CloudKind::ToweringCumulus.storminess() > 1.0);
    }
}
