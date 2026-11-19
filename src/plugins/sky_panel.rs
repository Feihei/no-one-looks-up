//! Sky tuning panel (R5.1): live, keyboard-driven control of the golden-hour
//! look.
//!
//! F5 opens it; Up/Down picks a row, Left/Right nudges the value. Every change
//! writes `DuskSky`, which the sky systems already treat as the single
//! source of truth, so nothing here is a preview-only path — what you tune in
//! the panel is what the game renders.
//!
//! Why a panel at all: cloud and dusk tuning is iterative and the parameters
//! interact (cover changes the cloud type, humidity changes it again, exposure
//! changes how much of the result you can see). Recompiling between attempts
//! made that loop too slow to do well.
//!
//! Scope: only parameters bevy_elements exposes through `Weather`,
//! `CloudLayer` or the camera are listed. Cloud density, noise scale and march
//! step count are compiled into the crate's shaders, so they are NOT here —
//! changing those means patching the dependency, which is a separate decision.

use bevy::prelude::*;

use crate::core::input::{
    DEV_PANEL_DOWN, DEV_PANEL_LEFT, DEV_PANEL_RIGHT, DEV_PANEL_UP, DEV_TOGGLE_SKY_PANEL,
};
use crate::core::ui_font::UiFont;
use crate::plugins::sky::DuskSky;

/// Whether the panel is drawn and accepting keys.
#[derive(Resource, Debug, Default)]
pub struct SkyPanelState {
    pub open: bool,
    /// Selected row index into [`PARAMS`].
    pub selected: usize,
}

/// What a row edits.
pub enum SkyValue {
    /// A plain float with range and step.
    Float {
        get: fn(&DuskSky) -> f32,
        set: fn(&mut DuskSky, f32),
        step: f32,
        range: (f32, f32),
        decimals: usize,
    },
    /// The cloud kind (an enum): Left/Right cycles it, and `None` (clear sky)
    /// is part of the cycle.
    CloudKind,
    /// Cloud amount, which only has meaning when a kind is selected.
    CloudAmount,
    /// A cirrus setting: high wispy cloud, a separate cheap layer (R5.6).
    Cirrus(
        fn(&crate::plugins::cirrus::CirrusSettings) -> f32,
        fn(&mut crate::plugins::cirrus::CirrusSettings, f32),
        f32,
        (f32, f32),
    ),
}

/// One tunable row.
pub struct SkyParam {
    pub name: &'static str,
    pub value: SkyValue,
}

/// Every row the panel exposes, in display order.
///
/// Organised the way the sky is actually read: what cloud is there, how much,
/// then how it is lit.
pub const PARAMS: &[SkyParam] = &[
    SkyParam {
        name: "cloud kind",
        value: SkyValue::CloudKind,
    },
    SkyParam {
        name: "cloud amount",
        value: SkyValue::CloudAmount,
    },
    SkyParam {
        name: "layer base",
        value: SkyValue::Float {
            get: |sky| {
                sky.cloud_layer
                    .map(|(b, _)| b)
                    .unwrap_or_else(|| sky.cloud_kind.map(|k| k.layer().0).unwrap_or(1200.0))
            },
            set: |sky, v| {
                let top = sky
                    .cloud_layer
                    .map(|(_, t)| t)
                    .or_else(|| sky.cloud_kind.map(|k| k.layer().1))
                    .unwrap_or(v + 1400.0);
                sky.cloud_layer = Some((v, top.max(v + 200.0)));
            },
            step: 100.0,
            range: (300.0, 4000.0),
            decimals: 0,
        },
    },
    SkyParam {
        name: "layer top",
        value: SkyValue::Float {
            get: |sky| {
                sky.cloud_layer
                    .map(|(_, t)| t)
                    .unwrap_or_else(|| sky.cloud_kind.map(|k| k.layer().1).unwrap_or(2600.0))
            },
            set: |sky, v| {
                let base = sky
                    .cloud_layer
                    .map(|(b, _)| b)
                    .or_else(|| sky.cloud_kind.map(|k| k.layer().0))
                    .unwrap_or(1200.0);
                sky.cloud_layer = Some((base.min(v - 200.0), v.max(base + 200.0)));
            },
            step: 200.0,
            range: (500.0, 9000.0),
            decimals: 0,
        },
    },
    SkyParam {
        name: "humidity bias",
        value: SkyValue::Float {
            get: |sky| sky.humidity_bias,
            set: |sky, v| sky.humidity_bias = v,
            step: 0.02,
            range: (-0.5, 0.5),
            decimals: 2,
        },
    },
    SkyParam {
        name: "storm bias",
        value: SkyValue::Float {
            get: |sky| sky.storminess_bias,
            set: |sky, v| sky.storminess_bias = v,
            step: 0.05,
            range: (-1.0, 3.0),
            decimals: 2,
        },
    },
    SkyParam {
        name: "cirrus amount",
        value: SkyValue::Cirrus(|c| c.amount, |c, v| c.amount = v, 0.02, (0.0, 1.0)),
    },
    SkyParam {
        name: "cirrus stretch",
        value: SkyValue::Cirrus(|c| c.stretch, |c, v| c.stretch = v, 0.2, (1.0, 8.0)),
    },
    SkyParam {
        name: "cirrus scale",
        value: SkyValue::Cirrus(|c| c.scale, |c, v| c.scale = v, 0.1, (0.2, 12.0)),
    },
    SkyParam {
        name: "exposure ev",
        value: SkyValue::Float {
            get: |sky| sky.exposure_ev100,
            set: |sky, v| sky.exposure_ev100 = v,
            step: 0.1,
            range: (8.0, 18.0),
            decimals: 2,
        },
    },
    SkyParam {
        name: "bloom",
        value: SkyValue::Float {
            get: |sky| sky.bloom_intensity,
            set: |sky, v| sky.bloom_intensity = v,
            step: 0.02,
            range: (0.0, 1.5),
            decimals: 2,
        },
    },
    SkyParam {
        name: "exp jitter",
        value: SkyValue::Float {
            get: |sky| sky.exposure_noise_amplitude,
            set: |sky, v| sky.exposure_noise_amplitude = v,
            step: 0.01,
            range: (0.0, 0.6),
            decimals: 2,
        },
    },
];

/// Cycle order for the cloud-kind row: `None` (clear) first, then the kinds.
/// A clear sky must be reachable, not just a kind set to zero amount.
fn cloud_kind_cycle(
    current: Option<crate::plugins::sky::CloudKind>,
) -> Option<crate::plugins::sky::CloudKind> {
    use crate::plugins::sky::CloudKind;
    if current.is_none() {
        return Some(CloudKind::ALL[0]);
    }
    let index = CloudKind::ALL.iter().position(|k| Some(*k) == current);
    match index {
        Some(i) if i + 1 < CloudKind::ALL.len() => Some(CloudKind::ALL[i + 1]),
        // Past the last kind: back to a clear sky.
        _ => None,
    }
}

/// Marker on the panel's UI root.
#[derive(Component)]
struct SkyPanelRoot;

/// Reads input and updates `DuskSky` / [`SkyPanelState`].
fn sky_panel_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<SkyPanelState>,
    mut sky: ResMut<DuskSky>,
    mut cirrus: ResMut<crate::plugins::cirrus::CirrusSettings>,
) {
    if keys.just_pressed(DEV_TOGGLE_SKY_PANEL) {
        state.open = !state.open;
        info!(
            "sky panel {}",
            if state.open {
                "open (F5 to close, Up/Down select, Left/Right adjust)"
            } else {
                "closed"
            }
        );
        if !state.open {
            return;
        }
    }
    if !state.open {
        return;
    }

    if keys.just_pressed(DEV_PANEL_UP) {
        state.selected = (state.selected + PARAMS.len() - 1) % PARAMS.len();
    }
    if keys.just_pressed(DEV_PANEL_DOWN) {
        state.selected = (state.selected + 1) % PARAMS.len();
    }

    let mut delta = 0.0;
    if keys.just_pressed(DEV_PANEL_LEFT) {
        delta -= 1.0;
    }
    if keys.just_pressed(DEV_PANEL_RIGHT) {
        delta += 1.0;
    }
    // Held keys repeat: nudging a 50 m step one press at a time is unusable.
    if keys.pressed(DEV_PANEL_LEFT) || keys.pressed(DEV_PANEL_RIGHT) {
        // `just_pressed` already counted the first press; only add repeats
        // after the initial delay so a single tap stays one step.
        if keys.pressed(DEV_PANEL_LEFT) && !keys.just_pressed(DEV_PANEL_LEFT) {
            delta -= 1.0;
        }
        if keys.pressed(DEV_PANEL_RIGHT) && !keys.just_pressed(DEV_PANEL_RIGHT) {
            delta += 1.0;
        }
    }
    if delta == 0.0 {
        return;
    }

    let param = &PARAMS[state.selected];
    // Direction only: -1 or +1 (held keys already collapsed into this).
    let direction = if delta < 0.0 { -1.0 } else { 1.0 };
    // A repeat means the key was already down on the previous frame.
    let is_repeat = !(keys.just_pressed(DEV_PANEL_LEFT) || keys.just_pressed(DEV_PANEL_RIGHT));
    match &param.value {
        SkyValue::Float {
            get,
            set,
            step,
            range,
            ..
        } => {
            let next = (get(&sky) + direction * step).clamp(range.0, range.1);
            set(&mut sky, next);
            info!("{} = {}", param.name, format_row(param, &sky, &cirrus));
        }
        SkyValue::CloudKind => {
            // Cycling is discrete: a held key must not spin through every
            // kind in a few frames, so only a fresh press counts. The caller
            // adds repeat deltas for held keys, which is wrong here.
            if !is_repeat {
                let next = cloud_kind_cycle(sky.cloud_kind);
                sky.cloud_kind = next;
                if next.is_some() && sky.cloud_amount <= 0.001 {
                    // Picking a kind with amount 0 would look like "nothing
                    // happened"; give it a visible default.
                    sky.cloud_amount = 0.35;
                }
                info!("{} = {}", param.name, format_row(param, &sky, &cirrus));
            }
        }
        SkyValue::CloudAmount => {
            sky.cloud_amount = (sky.cloud_amount + direction * 0.02).clamp(0.0, 1.0);
            info!("{} = {}", param.name, format_row(param, &sky, &cirrus));
        }
        SkyValue::Cirrus(get, set, step, range) => {
            let next = (get(&cirrus) + direction * step).clamp(range.0, range.1);
            set(&mut cirrus, next);
            info!("{} = {}", param.name, format_row(param, &sky, &cirrus));
        }
    }
}

/// Renders a row's value for the panel and the log line.
fn format_row(
    param: &SkyParam,
    sky: &DuskSky,
    cirrus: &crate::plugins::cirrus::CirrusSettings,
) -> String {
    match &param.value {
        SkyValue::Float { get, decimals, .. } => format!("{:.*}", decimals, get(sky)),
        SkyValue::CloudKind => match sky.cloud_kind {
            Some(kind) => kind.label().to_string(),
            None => "clear (no cloud)".to_string(),
        },
        SkyValue::CloudAmount => {
            if sky.cloud_kind.is_some() {
                format!("{:.2}", sky.cloud_amount)
            } else {
                "-".to_string()
            }
        }
        SkyValue::Cirrus(get, _, _, _) => format!("{:.2}", get(cirrus)),
    }
}

/// Draws the panel text; the whole panel is rebuilt only when something
/// changed (there are ~9 lines, but rebuilding every frame is still waste).
fn sky_panel_ui(
    state: Res<SkyPanelState>,
    sky: Res<DuskSky>,
    cirrus: Res<crate::plugins::cirrus::CirrusSettings>,
    font: Res<UiFont>,
    mut commands: Commands,
    roots: Query<Entity, With<SkyPanelRoot>>,
    mut cached: Local<Option<(bool, usize, Vec<String>)>>,
) {
    let lines: Vec<String> = if state.open {
        PARAMS
            .iter()
            .enumerate()
            .map(|(index, param)| {
                let value = format_row(param, &sky, &cirrus);
                let marker = if index == state.selected { ">" } else { " " };
                format!("{marker} {:<14} {}", param.name, value)
            })
            .collect()
    } else {
        Vec::new()
    };

    let signature = (state.open, state.selected, lines.clone());
    if cached.as_ref() == Some(&signature) && !roots.is_empty() {
        return;
    }
    *cached = Some(signature);

    for root in &roots {
        commands.entity(root).try_despawn();
    }
    if !state.open {
        return;
    }

    commands
        .spawn((
            Name::new("SkyPanel"),
            SkyPanelRoot,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(18.0),
                top: Val::Px(18.0),
                padding: UiRect::all(Val::Px(12.0)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(3.0),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.03, 0.05, 0.72)),
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new("SKY TUNING  F5"),
                font.at(13.0),
                TextColor(Color::srgb(0.95, 0.85, 0.55)),
            ));
            for line in &lines {
                panel.spawn((
                    Text::new(line.clone()),
                    font.at(12.0),
                    TextColor(Color::srgb(0.88, 0.90, 0.92)),
                ));
            }
        });
}

pub struct SkyPanelPlugin;

impl Plugin for SkyPanelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<SkyPanelState>()
            .init_resource::<DuskSky>()
            // The panel is a text overlay like the menus; `UiFontPlugin`
            // supplies the face, the default covers harnesses without assets.
            .init_resource::<UiFont>()
            .add_systems(Update, (sky_panel_input, sky_panel_ui).chain());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::sky::CloudKind;

    fn app_with_panel() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<SkyPanelState>()
            .init_resource::<DuskSky>()
            .init_resource::<crate::plugins::cirrus::CirrusSettings>()
            .add_systems(Update, sky_panel_input);
        app
    }

    /// Press, run one frame, then release and clear the edge state.
    ///
    /// `MinimalPlugins` has no `InputPlugin`, so nothing calls `clear()` each
    /// frame and `just_pressed` would stay true forever — a second `update()`
    /// would act again. In the real game `keyboard_input_system` does this;
    /// here the test has to.
    fn tap(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        input.release(key);
        input.clear();
        app.update();
    }

    fn row(name: &str) -> usize {
        PARAMS
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| panic!("no row named {name}"))
    }

    #[test]
    fn panel_starts_closed_and_ignores_keys_until_opened() {
        let mut app = app_with_panel();
        let before = app.world().resource::<DuskSky>().exposure_ev100;
        tap(&mut app, DEV_PANEL_RIGHT);
        assert_eq!(app.world().resource::<DuskSky>().exposure_ev100, before);
        assert!(!app.world().resource::<SkyPanelState>().open);
    }

    #[test]
    fn f5_toggles_the_panel() {
        let mut app = app_with_panel();
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        assert!(app.world().resource::<SkyPanelState>().open);
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        assert!(!app.world().resource::<SkyPanelState>().open);
    }

    #[test]
    fn cloud_kind_cycles_through_every_kind_and_back_to_clear() {
        // A clear sky must be reachable by the same control, not only by
        // dragging the amount to zero: the game needs cloudless dusk too.
        let mut app = app_with_panel();
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        app.world_mut().resource_mut::<SkyPanelState>().selected = row("cloud kind");
        app.world_mut().resource_mut::<DuskSky>().cloud_kind = Some(CloudKind::ALL[0]);

        // Starting on ALL[0], it takes one press per remaining kind plus one
        // more to step off the end into the clear state. Seed the list with
        // the starting kind, or it looks like the cycle skipped it.
        let mut seen = vec![app.world().resource::<DuskSky>().cloud_kind];
        for _ in 0..CloudKind::ALL.len() {
            tap(&mut app, DEV_PANEL_RIGHT);
            seen.push(app.world().resource::<DuskSky>().cloud_kind);
        }

        for kind in CloudKind::ALL {
            assert!(seen.contains(&Some(kind)), "cycling never reached {kind:?}");
        }
        assert_eq!(
            seen.last().copied().flatten(),
            None,
            "cycling past the last kind must give a clear sky"
        );
        assert_eq!(
            seen[1].map(|k| k.label()),
            Some(CloudKind::ALL[1].label()),
            "the first press should advance to the next kind"
        );
    }

    #[test]
    fn picking_a_kind_gives_it_a_visible_amount() {
        // A kind at amount 0 renders nothing; selecting it would look broken.
        let mut app = app_with_panel();
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        app.world_mut().resource_mut::<SkyPanelState>().selected = row("cloud kind");
        {
            let mut sky = app.world_mut().resource_mut::<DuskSky>();
            sky.cloud_kind = None;
            sky.cloud_amount = 0.0;
        }
        tap(&mut app, DEV_PANEL_RIGHT);
        let sky = app.world().resource::<DuskSky>();
        assert!(sky.cloud_kind.is_some());
        assert!(
            sky.cloud_amount > 0.0,
            "selecting a kind must leave it visible, amount = {}",
            sky.cloud_amount
        );
    }

    #[test]
    fn clear_sky_resolves_to_no_cover() {
        // The clear state must actually clear: a stale cover would leave the
        // previous cloud sheet in the sky.
        let sky = DuskSky {
            cloud_kind: None,
            ..Default::default()
        };
        assert!(
            crate::plugins::sky::resolve_clouds(&sky).is_none(),
            "no kind must resolve to no cloud"
        );

        // Amount 0 with a kind keeps the kind but shows nothing.
        let sky = DuskSky {
            cloud_kind: Some(CloudKind::BrokenCumulus),
            cloud_amount: 0.0,
            ..Default::default()
        };
        assert_eq!(
            crate::plugins::sky::resolve_clouds(&sky).map(|c| c.cover),
            Some(0.0)
        );
    }

    #[test]
    fn every_kind_resolves_to_weather_the_crate_can_use() {
        for kind in CloudKind::ALL {
            let sky = DuskSky {
                cloud_kind: Some(kind),
                cloud_amount: 1.0,
                ..Default::default()
            };
            let clouds = crate::plugins::sky::resolve_clouds(&sky)
                .unwrap_or_else(|| panic!("{kind:?} resolved to nothing"));
            assert!(
                (0.0..=1.0).contains(&clouds.cover),
                "{kind:?}: cover {} out of range",
                clouds.cover
            );
            assert!(
                (0.0..=1.0).contains(&clouds.humidity),
                "{kind:?}: humidity {} out of range",
                clouds.humidity
            );
            assert!(
                (0.0..=6.0).contains(&clouds.storminess),
                "{kind:?}: storminess {} out of range",
                clouds.storminess
            );
            assert!(
                clouds.layer.1 > clouds.layer.0 + 100.0,
                "{kind:?}: layer {:?} is degenerate",
                clouds.layer
            );
            assert!(
                (0.0..=12.0).contains(&clouds.wind),
                "{kind:?}: wind {} out of the calm band",
                clouds.wind
            );
        }
    }

    #[test]
    fn layer_rows_keep_base_below_top() {
        let mut app = app_with_panel();
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        for name in ["layer base", "layer top"] {
            app.world_mut().resource_mut::<SkyPanelState>().selected = row(name);
            for _ in 0..120 {
                tap(&mut app, DEV_PANEL_RIGHT);
            }
            if let Some((base, top)) = app.world().resource::<DuskSky>().cloud_layer {
                assert!(
                    top > base,
                    "{name}: top {top} <= base {base} at the high end"
                );
            }
            for _ in 0..240 {
                tap(&mut app, DEV_PANEL_LEFT);
            }
            if let Some((base, top)) = app.world().resource::<DuskSky>().cloud_layer {
                assert!(
                    top > base,
                    "{name}: top {top} <= base {base} at the low end"
                );
            }
        }
    }

    #[test]
    fn float_rows_stay_in_range_and_finite() {
        // A NaN exposure blanks the whole frame, and a negative humidity feeds
        // the crate's smoothsteps garbage.
        let mut app = app_with_panel();
        tap(&mut app, DEV_TOGGLE_SKY_PANEL);
        for (index, param) in PARAMS.iter().enumerate() {
            let SkyValue::Float { get, range, .. } = &param.value else {
                continue;
            };
            app.world_mut().resource_mut::<SkyPanelState>().selected = index;
            for _ in 0..200 {
                tap(&mut app, DEV_PANEL_RIGHT);
            }
            let high = get(app.world().resource::<DuskSky>());
            for _ in 0..400 {
                tap(&mut app, DEV_PANEL_LEFT);
            }
            let low = get(app.world().resource::<DuskSky>());
            for (value, end) in [(high, "high"), (low, "low")] {
                assert!(value.is_finite(), "{} is non-finite at {end}", param.name);
                assert!(
                    value >= range.0 - 1e-3 && value <= range.1 + 1e-3,
                    "{} = {value} escaped {:?} at {end}",
                    param.name,
                    range
                );
            }
        }
    }
}
