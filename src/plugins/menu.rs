//! Menu UI plugin (R4.5/4.6).
//!
//! Front-end flow: the handwritten title (`assets/ui/title.png`, white ink
//! extracted from a photo) fades in over the world, waits for any key, fades
//! out, and hands over to the menu. The menu is keyboard-driven: up/down
//! moves the cursor, Enter confirms, Esc cancels back one level. Seed and
//! season are rows that open their own editor screens (left/right adjust,
//! digits type directly, Enter keeps, Esc reverts); Load opens a browser
//! over the hundred save slots (00-99, slot 00 the autosave). Returning to
//! the menu from a run skips the title — it plays once per boot.
//!
//! Starting a run goes through the R4.3 restore flow (a [`LoadRequest`]
//! with a fresh save), so the world rebuild, streaming reset and pose
//! handling stay single-sourced. Phase transitions own the cursor: playing
//! grabs it (Locked), every menu releases it.

use bevy::ecs::hierarchy::ChildOf;
use bevy::prelude::*;
use bevy::text::TextFont;
use bevy::ui::Node;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::core::game_state::{GamePhase, SeedInput};
use crate::core::input::PAUSE;
use crate::plugins::save::{AUTOSAVE_SLOT, LoadRequest, SaveData, Season, read_save, slot_info};
use crate::world::chunks::DEFAULT_SEED;
use crate::world::explored::{ExploredBitmap, WorldExplorationCompleted};
use crate::world::path_network::{WorldSeed, generate_world, spawn_point};
use crate::world::season::SeasonState;
use crate::world::world_center;

pub struct MenuPlugin;

impl Plugin for MenuPlugin {
    fn build(&self, app: &mut App) {
        // The title art is optional: headless tests have no `AssetServer`,
        // and the title screen falls back to plain text without it.
        let title_art = app
            .world()
            .get_resource::<AssetServer>()
            .map(|server| server.load("ui/title.png"));
        app.init_resource::<GamePhase>()
            .init_resource::<SeedInput>()
            .init_resource::<MenuUiRoot>()
            .init_resource::<MenuSelection>()
            .init_resource::<MenuScreen>()
            .init_resource::<TitleFade>()
            .init_resource::<LoadBrowser>()
            .insert_resource(TitleArt(title_art))
            .add_message::<crate::world::explored::WorldExplorationCompleted>()
            .add_systems(
                PreUpdate,
                (
                    // The title is a fade, not a key menu: any key dismisses it.
                    title_input,
                    title_fade,
                    menu_input,
                    sync_ui_root,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    title_visuals,
                    update_hud,
                    cursor_by_phase,
                    // Completion watcher runs after exploration tracking.
                    watch_exploration_complete.after(crate::world::explored::track_player_chunk),
                ),
            );
    }
}

/// Which front-end screen is showing (only meaningful while
/// [`GamePhase::MainMenu`]).
#[derive(Resource, Debug, Clone, PartialEq, Default)]
enum MenuScreen {
    /// The handwriting title: fades in, waits for any key, fades out.
    #[default]
    Title,
    /// Seed / season / start / load / quit.
    Root,
    /// Editing the seed; `original` is the buffer before entering.
    SeedEdit { original: String },
    /// Editing the season; `original` is the season before entering.
    SeasonEdit { original: Season },
    /// Browsing the hundred save slots (00-99) to load.
    Load,
    /// Choosing a slot to save into (opened from the pause menu's Save row).
    SavePick,
}

/// Fade animation of the title screen.
#[derive(Resource, Debug, Default)]
struct TitleFade {
    alpha: f32,
    state: TitleFadeState,
}

#[derive(Debug, Default, PartialEq)]
enum TitleFadeState {
    /// Rising alpha; any key skips straight to [`TitleFadeState::Out`].
    #[default]
    In,
    /// Fully visible, waiting for a key.
    Hold,
    /// Falling alpha; completion opens the menu.
    Out,
    Done,
}

const TITLE_FADE_IN_SECS: f32 = 2.5;
const TITLE_FADE_OUT_SECS: f32 = 1.2;

/// Width/height of the title art (rotated upright in the extraction script).
const TITLE_ART_ASPECT: f32 = 1.96;

/// The processed handwriting art (`assets/ui/title.png`); `None` when there
/// is no asset server (headless tests) — the title falls back to text.
#[derive(Resource, Debug, Default)]
struct TitleArt(Option<Handle<Image>>);

/// Marker for the title's hint text so the fade can tint it.
#[derive(Component, Debug)]
struct TitleHint;

/// The load screen's browsing state: which slot is shown and any digits
/// typed toward a jump.
#[derive(Resource, Debug, Default)]
struct LoadBrowser {
    /// Slot under the cursor while no digits are being typed (0-99).
    slot: u32,
    /// Digits typed so far (at most two); while non-empty they override
    /// the stepped cursor.
    typed: String,
}

/// Slots run 00-99 (ADR-0001: slot 0 is the autosave).
const SLOT_COUNT: u32 = 100;

impl LoadBrowser {
    /// The slot the player currently points at: typed digits win over the
    /// stepped cursor.
    fn target(&self) -> u32 {
        if self.typed.is_empty() {
            self.slot
        } else {
            self.typed.parse::<u32>().unwrap_or(0) % SLOT_COUNT
        }
    }
}

/// Highlighted row of the root menu (an index into [`root_rows`]). `phase`
/// remembers which phase the index belongs to, so entering a different
/// phase resets the cursor to its first row.
#[derive(Resource, Debug, Default)]
struct MenuSelection {
    index: usize,
    phase: GamePhase,
}

/// Root entity of whichever menu/HUD screen is showing (despawned on phase
/// change; `None` when the world is playing bare).
#[derive(Resource, Debug, Default)]
struct MenuUiRoot {
    entity: Option<Entity>,
}

/// What confirming a highlighted menu row does (root menu and pause menu).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    EditSeed,
    EditSeason,
    Start,
    OpenLoad,
    Quit,
    Resume,
    OpenSave,
    BackToMenu,
}

/// The pause menu's rows in display order.
fn pause_rows() -> Vec<(String, MenuAction)> {
    vec![
        ("Resume".to_string(), MenuAction::Resume),
        ("Save".to_string(), MenuAction::OpenSave),
        ("Main menu".to_string(), MenuAction::BackToMenu),
    ]
}

/// Cursor highlight of the selected row.
const SELECTED_COLOR: Color = Color::srgb(1.0, 0.84, 0.35);

/// Dim gray of the per-screen control hints.
const HINT_COLOR: Color = Color::srgb(0.62, 0.62, 0.62);

/// Digit keys in typing order (shared by the seed editor and the slot jump).
const DIGIT_KEYS: [(KeyCode, char); 10] = [
    (KeyCode::Digit0, '0'),
    (KeyCode::Digit1, '1'),
    (KeyCode::Digit2, '2'),
    (KeyCode::Digit3, '3'),
    (KeyCode::Digit4, '4'),
    (KeyCode::Digit5, '5'),
    (KeyCode::Digit6, '6'),
    (KeyCode::Digit7, '7'),
    (KeyCode::Digit8, '8'),
    (KeyCode::Digit9, '9'),
];

/// The root menu's rows in display order: label + confirm action.
fn root_rows(seed_display: &str, season_label: &str) -> Vec<(String, MenuAction)> {
    vec![
        (format!("Seed: {seed_display}"), MenuAction::EditSeed),
        (format!("Season: {season_label}"), MenuAction::EditSeason),
        ("Start".to_string(), MenuAction::Start),
        ("Load".to_string(), MenuAction::OpenLoad),
        ("Quit".to_string(), MenuAction::Quit),
    ]
}

/// The seed the run would use right now: the typed buffer, else the default.
fn effective_seed(seed_input: &SeedInput) -> String {
    if seed_input.0.is_empty() {
        DEFAULT_SEED.to_string()
    } else {
        seed_input.0.clone()
    }
}

/// Steps the seed buffer by `delta`, saturating at the u64 bounds. An empty
/// buffer counts as the default seed (typing a digit replaces the working
/// number instead: the buffer starts the editor empty).
fn bump_seed(buffer: &mut String, delta: i64) {
    let value = buffer.parse::<u64>().unwrap_or(DEFAULT_SEED);
    let bumped = if delta < 0 {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta.unsigned_abs())
    };
    *buffer = bumped.to_string();
}

/// One full-screen centred panel (menus and the title share the layout).
fn panel() -> Node {
    Node {
        position_type: PositionType::Absolute,
        width: Val::Percent(100.0),
        height: Val::Percent(100.0),
        display: Display::Flex,
        flex_direction: FlexDirection::Column,
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        row_gap: Val::Px(8.0),
        ..default()
    }
}

/// The in-game status line, pinned to the top-left corner.
///
/// It used to reuse the full-screen centred panel, which put a line of
/// exploration text across the middle of the view -- straight through the
/// thing the game exists to show. The HUD is status, not a menu: it hugs a
/// corner, stays small, and never competes with the sky.
fn hud_node() -> Node {
    Node {
        position_type: PositionType::Absolute,
        top: Val::Px(12.0),
        left: Val::Px(14.0),
        ..default()
    }
}

fn text_node(line: &str, size: f32) -> (Text, TextFont, TextColor) {
    (
        Text::new(line.to_string()),
        TextFont::from_font_size(size),
        TextColor(Color::WHITE),
    )
}

/// The HUD's exploration readout.
fn hud_line(bitmap: &ExploredBitmap) -> String {
    format!(
        "Explored: {} / {} ({:.1}%)",
        bitmap.len(),
        bitmap.total(),
        bitmap.fraction() * 100.0
    )
}

/// Text content of one menu screen: static header lines, selectable rows,
/// control hints.
fn screen_panels(
    screen: &MenuScreen,
    seed_input: &SeedInput,
    season: &SeasonState,
    browser: &LoadBrowser,
    bitmap: &ExploredBitmap,
) -> (Vec<String>, Vec<(String, MenuAction)>, Vec<String>) {
    match screen {
        MenuScreen::Root => (
            vec!["NO ONE LOOKS UP".to_string(), String::new()],
            root_rows(&effective_seed(seed_input), season.season.label()),
            vec!["arrows select, Enter confirms, Esc quits".to_string()],
        ),
        MenuScreen::SeedEdit { .. } => (
            vec![
                "SEED".to_string(),
                String::new(),
                effective_seed(seed_input),
            ],
            Vec::new(),
            vec!["left/right adjust, digits type, Enter ok, Esc cancels".to_string()],
        ),
        MenuScreen::SeasonEdit { .. } => (
            vec![
                "SEASON".to_string(),
                String::new(),
                season.season.label().to_string(),
            ],
            Vec::new(),
            vec!["left/right change, Enter ok, Esc cancels".to_string()],
        ),
        MenuScreen::Load => slot_pick_panels("LOAD", browser, bitmap, false),
        // Unreachable here: the title screen spawns its own panel (image +
        // fading hint) and SavePick only exists in the pause flow, whose
        // panel sync_ui_root builds directly.
        MenuScreen::Title | MenuScreen::SavePick => (Vec::new(), Vec::new(), Vec::new()),
    }
}

/// Text content of the slot picker (the load browser and the pause flow's
/// save picker share it): header lines and control hints, no rows — the
/// whole screen is one picker.
fn slot_pick_panels(
    title: &str,
    browser: &LoadBrowser,
    bitmap: &ExploredBitmap,
    saving: bool,
) -> (Vec<String>, Vec<(String, MenuAction)>, Vec<String>) {
    let target = browser.target();
    let slot_line = if browser.typed.is_empty() {
        format!("Slot {target:02}")
    } else {
        format!("Slot {:_<2}", browser.typed)
    };
    let info_line = match slot_info(target) {
        Some(info) => {
            let tag = if target == AUTOSAVE_SLOT {
                " (autosave)"
            } else if saving {
                " (overwrite)"
            } else {
                ""
            };
            format!(
                "seed {} | {} | {}/{} chunks{}",
                info.seed,
                info.season.label(),
                info.explored_chunks,
                bitmap.total(),
                tag
            )
        }
        None if saving => "empty - Enter saves here".to_string(),
        None => "empty".to_string(),
    };
    let hint = if saving {
        "left/right slot, digits jump, Enter saves, Esc back"
    } else {
        "left/right slot, digits jump, Enter loads, Esc back"
    };
    (
        vec![title.to_string(), String::new(), slot_line, info_line],
        Vec::new(),
        vec![hint.to_string()],
    )
}

/// Any key while the title shows starts fading it out.
fn title_input(
    keys: Res<ButtonInput<KeyCode>>,
    phase: Res<GamePhase>,
    screen: Res<MenuScreen>,
    mut fade: ResMut<TitleFade>,
) {
    if *phase != GamePhase::MainMenu || *screen != MenuScreen::Title {
        return;
    }
    if fade.state == TitleFadeState::Out || fade.state == TitleFadeState::Done {
        return;
    }
    if keys.get_just_pressed().next().is_some() {
        fade.state = TitleFadeState::Out;
    }
}

/// Advances the title fade; fade-out completion opens the menu.
fn title_fade(
    time: Res<Time>,
    phase: Res<GamePhase>,
    mut screen: ResMut<MenuScreen>,
    mut fade: ResMut<TitleFade>,
) {
    if *phase != GamePhase::MainMenu || fade.state == TitleFadeState::Done {
        return;
    }
    match fade.state {
        TitleFadeState::In => {
            fade.alpha = (fade.alpha + time.delta_secs() / TITLE_FADE_IN_SECS).min(1.0);
            if fade.alpha >= 1.0 {
                fade.state = TitleFadeState::Hold;
            }
        }
        TitleFadeState::Hold => {}
        TitleFadeState::Out => {
            fade.alpha = (fade.alpha - time.delta_secs() / TITLE_FADE_OUT_SECS).max(0.0);
            if fade.alpha <= 0.0 {
                fade.state = TitleFadeState::Done;
                *screen = MenuScreen::Root;
            }
        }
        TitleFadeState::Done => {}
    }
}

/// Applies the fade alpha to the title art and hint text every frame.
fn title_visuals(
    phase: Res<GamePhase>,
    screen: Res<MenuScreen>,
    fade: Res<TitleFade>,
    root: Res<MenuUiRoot>,
    mut images: Query<(&ChildOf, &mut ImageNode)>,
    mut hints: Query<&mut TextColor, With<TitleHint>>,
) {
    if *phase != GamePhase::MainMenu || *screen != MenuScreen::Title {
        return;
    }
    let Some(entity) = root.entity else {
        return;
    };
    let tint = Color::WHITE.with_alpha(fade.alpha);
    for (parent, mut image) in &mut images {
        if parent.0 == entity {
            image.color = tint;
        }
    }
    for mut color in &mut hints {
        color.0 = tint;
    }
}

/// Replaces the on-screen UI with the panel for the current phase/screen.
#[allow(clippy::too_many_arguments)]
fn sync_ui_root(
    phase: Res<GamePhase>,
    seed_input: Res<SeedInput>,
    season: Res<SeasonState>,
    bitmap: Res<ExploredBitmap>,
    screen: Res<MenuScreen>,
    browser: Res<LoadBrowser>,
    art: Res<TitleArt>,
    mut selection: ResMut<MenuSelection>,
    mut root: ResMut<MenuUiRoot>,
    mut commands: Commands,
    mut completed: MessageReader<WorldExplorationCompleted>,
) {
    if !phase.is_changed()
        && !seed_input.is_changed()
        && !season.is_changed()
        && !screen.is_changed()
        && !browser.is_changed()
        && root.entity.is_some()
        && !bitmap.is_changed()
    {
        // The HUD refreshes below; only content changes rebuild panels.
        if completed.read().next().is_none() {
            return;
        }
    }
    // Entering a different phase always lands the cursor on its first row
    // (mirrors the reset in `menu_input`, so the panel is never stale).
    if selection.phase != *phase {
        selection.phase = *phase;
        selection.index = 0;
    }
    if let Some(old) = root.entity.take() {
        commands.entity(old).try_despawn();
    }

    // The playing phase gets the corner HUD; every other phase is a
    // full-screen panel where a centred layout is correct.
    let entity = if *phase == GamePhase::Playing {
        commands
            .spawn(hud_node())
            .with_children(|parent| {
                // HUD text is deliberately small: it is a status readout, not
                // something to read while walking.
                parent.spawn(text_node(&hud_line(&bitmap), 14.0));
            })
            .id()
    } else if *phase == GamePhase::Paused {
        if *screen == MenuScreen::SavePick {
            let (headers, rows, hints) = slot_pick_panels("SAVE", &browser, &bitmap, true);
            spawn_text_panel(&mut commands, &headers, &rows, &hints, selection.index)
        } else {
            spawn_text_panel(
                &mut commands,
                &["PAUSED".to_string(), String::new()],
                &pause_rows(),
                &["arrows select, Enter confirms, Esc resumes".to_string()],
                selection.index,
            )
        }
    } else {
        match &*screen {
            MenuScreen::Title => spawn_title_panel(&mut commands, &art),
            screen => {
                let (headers, rows, hints) =
                    screen_panels(screen, &seed_input, &season, &browser, &bitmap);
                spawn_text_panel(&mut commands, &headers, &rows, &hints, selection.index)
            }
        }
    };
    root.entity = Some(entity);
}

/// Spawns a text panel: header lines, cursor rows, hint lines.
fn spawn_text_panel(
    commands: &mut Commands,
    headers: &[String],
    rows: &[(String, MenuAction)],
    hints: &[String],
    cursor: usize,
) -> Entity {
    commands
        .spawn(panel())
        .with_children(|parent| {
            for (line_no, line) in headers.iter().enumerate() {
                let size = if line_no == 0 { 48.0 } else { 24.0 };
                parent.spawn(text_node(line, size));
            }
            for (row, (label, _)) in rows.iter().enumerate() {
                let selected = row == cursor;
                let marker = if selected { "> " } else { "  " };
                let color = if selected {
                    SELECTED_COLOR
                } else {
                    Color::WHITE
                };
                parent.spawn((
                    Text::new(format!("{marker}{label}")),
                    TextFont::from_font_size(24.0),
                    TextColor(color),
                ));
            }
            for hint in hints {
                parent.spawn((
                    Text::new(hint.clone()),
                    TextFont::from_font_size(16.0),
                    TextColor(HINT_COLOR),
                ));
            }
        })
        .id()
}

/// Spawns the title screen: the handwriting art (or a text stand-in when no
/// asset server / file is available) and the fading hint.
fn spawn_title_panel(commands: &mut Commands, art: &TitleArt) -> Entity {
    let art_handle = art.0.clone();
    commands
        .spawn(panel())
        .with_children(|parent| {
            match art_handle {
                Some(handle) => {
                    parent.spawn((
                        ImageNode::new(handle),
                        Node {
                            // Landscape art: size by width, keep the ratio.
                            width: Val::Vh(56.0),
                            height: Val::Vh(56.0 / TITLE_ART_ASPECT),
                            ..default()
                        },
                    ));
                }
                None => {
                    // No art (headless tests, missing file): plain text
                    // stands in for the handwriting.
                    parent.spawn(text_node("NO ONE LOOKS UP", 48.0));
                }
            }
            parent.spawn((
                Text::new("press any key"),
                TextFont::from_font_size(20.0),
                TextColor(Color::WHITE.with_alpha(0.0)),
                TitleHint,
            ));
        })
        .id()
}

/// Starts a fresh run from the typed (or default) seed through the R4.3
/// restore flow.
fn start_run(
    seed_input: &mut SeedInput,
    season: &SeasonState,
    phase: &mut GamePhase,
    commands: &mut Commands,
) {
    let seed = seed_input.0.parse::<u64>().unwrap_or(DEFAULT_SEED);
    // R3.20 (2026-11-19 user decision: 出生点随机指定一个路口点): a fresh run
    // spawns on a seed-picked junction, never inside a block. Same
    // derivation the restore flow performs (handle_load_request regenerates
    // the same graph from the same seed), so the pose restored one frame
    // later IS this junction pose. The double generation costs well under
    // a second of pure CPU on the menu confirm.
    let (pose, yaw) = match generate_world(seed) {
        Ok((_, graph)) => {
            let (pos, yaw) = spawn_point(&graph, seed);
            ([pos.x, pos.y], yaw)
        }
        Err(_) => ([world_center(), world_center()], 0.0),
    };
    let fresh = SaveData::capture(seed, season.season, &ExploredBitmap::default(), pose, yaw);
    commands.insert_resource(WorldSeed(seed));
    commands.queue(|world: &mut World| {
        world.write_message(LoadRequest(fresh));
    });
    *phase = GamePhase::Playing;
    seed_input.0.clear();
}

/// Shared slot-picker input: left/right steps 00-99, typed digits jump
/// (two max, a third starts over), backspace trims.
fn picker_input(keys: &ButtonInput<KeyCode>, browser: &mut LoadBrowser) {
    if keys.just_pressed(KeyCode::ArrowLeft) {
        browser.slot = (browser.slot + SLOT_COUNT - 1) % SLOT_COUNT;
        browser.typed.clear();
    }
    if keys.just_pressed(KeyCode::ArrowRight) {
        browser.slot = (browser.slot + 1) % SLOT_COUNT;
        browser.typed.clear();
    }
    for (key, digit) in DIGIT_KEYS {
        if keys.just_pressed(key) {
            if browser.typed.len() >= 2 {
                browser.typed.clear();
            }
            browser.typed.push(digit);
        }
    }
    if keys.just_pressed(KeyCode::Backspace) {
        browser.typed.pop();
    }
}

/// Captures the running game state for a save (the pause flow's Save row).
fn capture_current(
    season: &SeasonState,
    bitmap: &ExploredBitmap,
    players: &Query<&Transform, With<Camera3d>>,
    seed: u64,
) -> SaveData {
    let pose = players
        .single()
        .map(|t| [t.translation.x, t.translation.z])
        .unwrap_or([world_center(), world_center()]);
    SaveData::capture(seed, season.season, bitmap, pose, 0.0)
}

/// Keyboard input driving the phase state machine and the menu screens.
/// Rows move with the arrow keys and confirm with Enter; Esc cancels back
/// one level.
#[allow(clippy::too_many_arguments)]
fn menu_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut phase: ResMut<GamePhase>,
    mut seed_input: ResMut<SeedInput>,
    mut season: ResMut<SeasonState>,
    mut selection: ResMut<MenuSelection>,
    mut screen: ResMut<MenuScreen>,
    mut browser: ResMut<LoadBrowser>,
    world_seed: Option<Res<WorldSeed>>,
    mut commands: Commands,
    players: Query<&Transform, With<Camera3d>>,
    bitmap: Res<ExploredBitmap>,
) {
    // The pause toggle is phase-bound: Esc only pauses while playing.
    let mut paused_this_frame = false;
    // Esc inside the save picker is consumed there (back to the pause
    // panel); without this flag the resume shortcut at the bottom would
    // fire on the same press and leave the pause menu entirely.
    let mut esc_consumed = false;
    if *phase == GamePhase::Playing && keys.just_pressed(PAUSE) {
        *phase = GamePhase::Paused;
        paused_this_frame = true;
    }

    // Entering a different phase resets the cursor and normalizes the
    // screen. This runs even on the frame a run starts, so a stale editor or
    // slot browser from the menu can never resurface: pausing always lands
    // on the pause panel, and returning to the main menu always lands on its
    // root rows (the title plays once per boot and never replays).
    if selection.phase != *phase {
        match *phase {
            GamePhase::MainMenu | GamePhase::Paused => *screen = MenuScreen::Root,
            GamePhase::Playing => {}
        }
        selection.phase = *phase;
        selection.index = 0;
    }

    if *phase == GamePhase::Playing || paused_this_frame {
        return;
    }

    // The front-end screens only listen while the main menu owns the show;
    // the pause menu has its own rows below.
    if *phase == GamePhase::MainMenu {
        // Clone to keep the match arms free to reassign `screen` (the editors
        // carry their revert point inside the screen value).
        let current = screen.clone();
        match &current {
            MenuScreen::Title => {} // the fade systems own this screen
            MenuScreen::Root => {
                let rows = root_rows(&effective_seed(&seed_input), season.season.label());
                if selection.index >= rows.len() {
                    selection.index = 0;
                }
                if keys.just_pressed(KeyCode::ArrowDown) {
                    selection.index = (selection.index + 1) % rows.len();
                }
                if keys.just_pressed(KeyCode::ArrowUp) {
                    selection.index = (selection.index + rows.len() - 1) % rows.len();
                }
                if keys.just_pressed(KeyCode::Enter) {
                    match rows[selection.index].1 {
                        MenuAction::EditSeed => {
                            *screen = MenuScreen::SeedEdit {
                                original: seed_input.0.clone(),
                            };
                        }
                        MenuAction::EditSeason => {
                            *screen = MenuScreen::SeasonEdit {
                                original: season.season,
                            };
                        }
                        MenuAction::Start => {
                            start_run(&mut seed_input, &season, &mut phase, &mut commands)
                        }
                        MenuAction::OpenLoad => {
                            *browser = LoadBrowser::default();
                            *screen = MenuScreen::Load;
                        }
                        MenuAction::Quit => std::process::exit(0),
                        // Root rows never resolve to pause actions.
                        MenuAction::Resume | MenuAction::OpenSave | MenuAction::BackToMenu => {}
                    }
                }
                if keys.just_pressed(PAUSE) {
                    std::process::exit(0);
                }
            }
            MenuScreen::SeedEdit { original } => {
                // Digits append, backspace trims, left/right steps the value.
                for (key, digit) in DIGIT_KEYS {
                    if keys.just_pressed(key) && seed_input.0.len() < 20 {
                        seed_input.0.push(digit);
                    }
                }
                if keys.just_pressed(KeyCode::Backspace) {
                    seed_input.0.pop();
                }
                if keys.just_pressed(KeyCode::ArrowLeft) {
                    bump_seed(&mut seed_input.0, -1);
                }
                if keys.just_pressed(KeyCode::ArrowRight) {
                    bump_seed(&mut seed_input.0, 1);
                }
                if keys.just_pressed(KeyCode::Enter) {
                    *screen = MenuScreen::Root;
                }
                if keys.just_pressed(PAUSE) {
                    seed_input.0 = original.clone();
                    *screen = MenuScreen::Root;
                }
            }
            MenuScreen::SeasonEdit { original } => {
                if keys.just_pressed(KeyCode::ArrowLeft) {
                    season.season = season.season.prev();
                }
                if keys.just_pressed(KeyCode::ArrowRight) {
                    season.season = season.season.next();
                }
                if keys.just_pressed(KeyCode::Enter) {
                    *screen = MenuScreen::Root;
                }
                if keys.just_pressed(PAUSE) {
                    season.season = *original;
                    *screen = MenuScreen::Root;
                }
            }
            MenuScreen::Load => {
                // Left/right steps 00-99; digits jump; Enter loads; Esc back.
                picker_input(&keys, &mut browser);
                if keys.just_pressed(KeyCode::Enter) {
                    // Empty slots do nothing; a valid save restores through
                    // the R4.3 flow.
                    let target = browser.target();
                    if let Some(info) = slot_info(target)
                        && let Ok(save) = read_save(info.slot)
                    {
                        commands.queue(move |world: &mut World| {
                            world.write_message(LoadRequest(save));
                        });
                        *phase = GamePhase::Playing;
                    }
                }
                if keys.just_pressed(PAUSE) {
                    *screen = MenuScreen::Root;
                }
            }
            // SavePick only exists in the pause flow; the main menu never
            // shows it.
            MenuScreen::SavePick => {}
        }
    } else if *phase == GamePhase::Paused {
        if *screen == MenuScreen::SavePick {
            // The save picker shares the load browser's input; Enter writes
            // the running game into the chosen slot, Esc backs out to the
            // pause panel.
            picker_input(&keys, &mut browser);
            if keys.just_pressed(KeyCode::Enter) {
                let slot = browser.target();
                let seed = world_seed.as_deref().map(|s| s.0).unwrap_or(DEFAULT_SEED);
                let data = capture_current(&season, &bitmap, &players, seed);
                match crate::plugins::save::write_save(slot, &data) {
                    Ok(()) => info!("saved to slot {slot:02}"),
                    Err(err) => error!("save failed: {err}"),
                }
                *screen = MenuScreen::Root;
            }
            if keys.just_pressed(PAUSE) {
                *screen = MenuScreen::Root;
                esc_consumed = true;
            }
        } else {
            let rows = pause_rows();
            if selection.index >= rows.len() {
                selection.index = 0;
            }
            if keys.just_pressed(KeyCode::ArrowDown) {
                selection.index = (selection.index + 1) % rows.len();
            }
            if keys.just_pressed(KeyCode::ArrowUp) {
                selection.index = (selection.index + rows.len() - 1) % rows.len();
            }
            if keys.just_pressed(KeyCode::Enter) {
                match rows[selection.index].1 {
                    MenuAction::Resume => *phase = GamePhase::Playing,
                    MenuAction::OpenSave => {
                        // 00 is the autosave; manual saves start at 01.
                        *browser = LoadBrowser {
                            slot: 1,
                            typed: String::new(),
                        };
                        *screen = MenuScreen::SavePick;
                    }
                    MenuAction::BackToMenu => *phase = GamePhase::MainMenu,
                    // Pause rows never resolve to root actions.
                    MenuAction::EditSeed
                    | MenuAction::EditSeason
                    | MenuAction::Start
                    | MenuAction::OpenLoad
                    | MenuAction::Quit => {}
                }
            }
        }
    }

    // Pause-menu shortcuts stay: Esc resumes, Backspace abandons the run to
    // the main menu.
    if *phase == GamePhase::Paused && !esc_consumed {
        if keys.just_pressed(PAUSE) {
            *phase = GamePhase::Playing;
        }
        // M now toggles the minimap while walking, so the pause menu keeps
        // Backspace for returning to the main menu.
        if keys.just_pressed(KeyCode::Backspace) {
            *phase = GamePhase::MainMenu;
        }
    }
}

/// Owns the cursor per phase: playing grabs it (Locked), menus release it.
fn cursor_by_phase(
    phase: Res<GamePhase>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    if !phase.is_changed() {
        return;
    }
    let Ok(mut options) = windows.single_mut() else {
        return;
    };
    if phase.is_game_active() {
        options.grab_mode = CursorGrabMode::Locked;
        options.visible = false;
    } else {
        options.grab_mode = CursorGrabMode::None;
        options.visible = true;
    }
}

/// Refreshes the HUD's exploration text (the HUD lives only while playing).
fn update_hud(
    phase: Res<GamePhase>,
    bitmap: Res<ExploredBitmap>,
    root: Res<MenuUiRoot>,
    mut texts: Query<(&ChildOf, &mut Text)>,
) {
    if !phase.is_game_active() {
        return;
    }
    let Some(entity) = root.entity else {
        return;
    };
    for (parent, mut text) in &mut texts {
        if parent.0 != entity {
            continue;
        }
        // While playing the panel only holds the HUD line.
        **text = hud_line(&bitmap);
    }
}

/// Fires the completion message once the whole world is explored (R4.6).
fn watch_exploration_complete(
    bitmap: Res<ExploredBitmap>,
    mut completed: Local<bool>,
    mut writer: MessageWriter<WorldExplorationCompleted>,
) {
    if *completed || !bitmap.is_changed() || !bitmap.is_complete() {
        return;
    }
    *completed = true;
    writer.write(WorldExplorationCompleted);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::PlayerBody;
    use crate::plugins::save::{save_path, write_save};
    use bevy::MinimalPlugins;

    fn app_with_ui() -> App {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            crate::world::chunks::WorldPlugin,
            crate::plugins::save::SavePlugin,
            MenuPlugin,
        ))
        .init_resource::<ButtonInput<KeyCode>>()
        // The menu picks the season, so it reads `SeasonState`; the real app
        // gets this from `SeasonPlugin`.
        .init_resource::<SeasonState>();
        app.world_mut().spawn((
            Camera3d::default(),
            PlayerBody::default(),
            Transform::from_xyz(world_center(), crate::player::EYE_HEIGHT, world_center()),
        ));
        app
    }

    /// Press, run one frame, then release and clear the edge state.
    ///
    /// `ButtonInput::clear()` keeps the `pressed` set, so a key that is not
    /// released stays "down" forever and a second `press()` of it is a no-op.
    /// The same dance as `sky_panel`'s `tap`: press, update, release, clear.
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

    /// Skips the title screen and lands on the root menu: tap a key, then
    /// force the fade to complete (real seconds are impractical in tests).
    fn past_title(app: &mut App) {
        tap(app, KeyCode::Space);
        *app.world_mut().resource_mut::<TitleFade>() = TitleFade {
            alpha: 0.0,
            state: TitleFadeState::Out,
        };
        app.update();
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
    }

    /// Moves the root menu cursor down `rows` times and confirms with Enter.
    fn pick(app: &mut App, rows: usize) {
        for _ in 0..rows {
            tap(app, KeyCode::ArrowDown);
        }
        tap(app, KeyCode::Enter);
    }

    /// The run starts from the third root row (Seed, Season, Start, ...).
    fn start_a_run(app: &mut App) {
        past_title(app);
        pick(app, 2);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Playing);
    }

    /// Serializes tests that touch the real saves directory against the
    /// save plugin's own tests.
    static SAVES_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_title_fades_and_any_key_opens_the_menu() {
        // Headless tests have no AssetServer, so the title falls back to
        // text — the flow is what is under test, not the image.
        let mut app = app_with_ui();
        app.update();
        app.update();
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Title);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::MainMenu);

        // Any key starts the fade-out; the fade completing opens the menu.
        tap(&mut app, KeyCode::Space);
        let fade = app.world().resource::<TitleFade>();
        assert!(
            matches!(fade.state, TitleFadeState::Out | TitleFadeState::Done),
            "any key dismisses the title"
        );
        *app.world_mut().resource_mut::<TitleFade>() = TitleFade {
            alpha: 0.0,
            state: TitleFadeState::Out,
        };
        app.update();
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);

        // The title panel is gone (no "press any key" hint anywhere).
        let mut query = app.world_mut().query::<&Text>();
        assert!(
            !query
                .iter(app.world())
                .any(|text| text.starts_with("press any key")),
            "the title hint must be despawned once the menu shows"
        );
    }

    #[test]
    fn main_menu_does_not_gate_the_world_inactive() {
        // The title builds its panel; the world is NOT active.
        let mut app = app_with_ui();
        app.update();
        let phase = *app.world().resource::<GamePhase>();
        assert_eq!(phase, GamePhase::MainMenu);
        assert!(!phase.is_game_active());
    }

    #[test]
    fn starting_sets_playing_and_hud_shows() {
        let mut app = app_with_ui();
        app.update();
        app.update();

        // Pick the Start row: a run starts (playing), the HUD panel replaces
        // the menu, and the world regenerates through the restore flow.
        start_a_run(&mut app);

        // The restore flow reset exploration to empty and the stream is
        // rebuilding around the spawn pose.
        let bitmap = app.world().resource::<ExploredBitmap>();
        assert!(!bitmap.is_empty(), "the restored world explores the start");
    }

    #[test]
    fn the_season_editor_picks_the_season_and_the_run_starts_with_it() {
        // ADR-0001: the menu is the only place a run's season is chosen. The
        // chain that matters is editor -> SeasonState -> the save the run
        // starts from, because the run's season is restored from that save.
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            crate::plugins::save::Season::default(),
            "the menu must open on the default season"
        );

        // Enter the season editor (second row) and step forward once.
        pick(&mut app, 1);
        assert!(matches!(
            *app.world().resource::<MenuScreen>(),
            MenuScreen::SeasonEdit { .. }
        ));
        tap(&mut app, KeyCode::ArrowRight);
        let picked = app.world().resource::<SeasonState>().season;
        assert_eq!(picked, crate::plugins::save::Season::default().next());

        // Confirm: back on the root menu with the picked season.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);

        // Start the run: the season must survive into the running world.
        // The cursor rests on the season row after confirming, so one Down
        // reaches Start.
        pick(&mut app, 1);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Playing);
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            picked,
            "the run must start with the season the editor showed"
        );
    }

    #[test]
    fn the_season_editor_esc_reverts() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);
        pick(&mut app, 1);

        // Step twice (Summer -> Autumn), then cancel: Summer must be back.
        tap(&mut app, KeyCode::ArrowRight);
        tap(&mut app, KeyCode::ArrowRight);
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            crate::plugins::save::Season::Summer,
            "Esc must revert the season to what it was on entry"
        );
    }

    #[test]
    fn the_seed_editor_types_a_seed_and_the_run_uses_it() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);

        // Enter the seed editor (first row); the buffer starts empty, so
        // typing builds a fresh number.
        pick(&mut app, 0);
        assert!(matches!(
            *app.world().resource::<MenuScreen>(),
            MenuScreen::SeedEdit { .. }
        ));
        tap(&mut app, KeyCode::Digit4);
        tap(&mut app, KeyCode::Digit2);
        assert_eq!(app.world().resource::<SeedInput>().0, "42");

        // Enter keeps the typed seed; starting the run uses it.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        pick(&mut app, 2);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Playing);
        assert_eq!(
            app.world().resource::<WorldSeed>().0,
            42,
            "the typed seed must reach the world"
        );
    }

    #[test]
    fn the_seed_editor_steps_and_esc_reverts() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);
        pick(&mut app, 0);

        // Left/right steps the effective seed (an empty buffer counts as the
        // default): right bumps the default by one, left steps back down.
        tap(&mut app, KeyCode::ArrowRight);
        assert_eq!(
            app.world().resource::<SeedInput>().0,
            (DEFAULT_SEED + 1).to_string(),
            "ArrowRight increments the effective seed"
        );
        tap(&mut app, KeyCode::ArrowLeft);
        assert_eq!(
            app.world().resource::<SeedInput>().0,
            DEFAULT_SEED.to_string()
        );

        // Esc cancels: the buffer is back to empty (the default) and no run
        // has started.
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        assert!(
            app.world().resource::<SeedInput>().0.is_empty(),
            "Esc must revert the seed buffer to what it was on entry"
        );
    }

    #[test]
    fn the_load_browser_steps_jumps_and_loads() {
        let _guard = SAVES_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);

        // Park one save in slot 5 to load.
        let mut bitmap = ExploredBitmap::default();
        bitmap.mark((3, 4));
        let data = crate::plugins::save::SaveData::capture(
            0xBEEF,
            crate::plugins::save::Season::Summer,
            &bitmap,
            [10.0, 20.0],
            0.0,
        );
        write_save(5, &data).expect("writes the parking save");

        // Enter the load browser (fourth row).
        pick(&mut app, 3);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Load);
        assert_eq!(app.world().resource::<LoadBrowser>().target(), 0);

        // Left wraps from 00 back to 99; typing jumps straight to 05.
        tap(&mut app, KeyCode::ArrowLeft);
        assert_eq!(
            app.world().resource::<LoadBrowser>().target(),
            SLOT_COUNT - 1
        );
        tap(&mut app, KeyCode::Digit0);
        tap(&mut app, KeyCode::Digit5);
        assert_eq!(app.world().resource::<LoadBrowser>().target(), 5);

        // Enter restores the parked save through the R4.3 flow.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Playing);
        assert_eq!(
            app.world().resource::<WorldSeed>().0,
            0xBEEF,
            "the browser must load the slot it points at"
        );
        assert!(app.world().resource::<ExploredBitmap>().is_explored((3, 4)));
        let _ = std::fs::remove_file(save_path(5));
    }

    #[test]
    fn the_load_browser_enters_an_empty_slot_safely() {
        let _guard = SAVES_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);
        pick(&mut app, 3);

        // An empty slot must not restore (and must not crash).
        let _ = std::fs::remove_file(save_path(7));
        tap(&mut app, KeyCode::Digit7);
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::MainMenu);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Load);
        let _ = std::fs::remove_file(save_path(7));
    }

    #[test]
    fn the_season_cannot_change_mid_run() {
        // ADR-0001: fixed for the run. The dev keys are gated on the phase, so
        // pressing them while walking must do nothing.
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);

        let before = app.world().resource::<SeasonState>().season;
        // Register the dev-shortcut system exactly as `SeasonPlugin` does and
        // drive it with the key held down, so the gate (not the missing
        // button) is what the test exercises.
        app.add_systems(bevy::prelude::Update, crate::plugins::season::dev_keys);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(crate::core::input::DEV_SEASON_WINTER);
        app.update();
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            before,
            "a mid-run season change would break the run's invariant"
        );

        // And the same key DOES work outside a run, so the gate is not simply
        // a dead system.
        *app.world_mut().resource_mut::<GamePhase>() = GamePhase::MainMenu;
        app.update();
        assert_eq!(
            app.world().resource::<SeasonState>().season,
            crate::plugins::save::Season::Winter,
            "the dev shortcut must still work from the menu"
        );
    }

    #[test]
    fn escape_pauses_and_resume_and_menu_return() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);

        // Esc: playing -> paused.
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Paused);

        // Backspace: back to the main menu (world stays alive underneath),
        // and the title does not replay.
        tap(&mut app, KeyCode::Backspace);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::MainMenu);
        assert_eq!(
            *app.world().resource::<MenuScreen>(),
            MenuScreen::Root,
            "returning from a run must skip the title"
        );
    }

    #[test]
    fn menu_ui_root_exists_per_phase() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        let root = app.world().resource::<MenuUiRoot>();
        assert!(root.entity.is_some(), "the title panel exists");
    }

    #[test]
    fn arrows_navigate_the_pause_menu_and_enter_confirms() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);

        // Esc pauses.
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Paused);

        // Down twice walks Resume -> Save -> Main menu.
        tap(&mut app, KeyCode::ArrowDown);
        tap(&mut app, KeyCode::ArrowDown);

        // Enter confirms the highlighted row: back to the main menu.
        tap(&mut app, KeyCode::Enter);
        assert_eq!(
            *app.world().resource::<GamePhase>(),
            GamePhase::MainMenu,
            "Enter must confirm the row the arrows highlighted"
        );
    }

    #[test]
    fn pause_save_picks_a_slot_and_writes_the_running_world() {
        let _guard = SAVES_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);

        // Esc pauses; the Save row opens the slot picker (default slot 01).
        tap(&mut app, PAUSE);
        tap(&mut app, KeyCode::ArrowDown);
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::SavePick);
        assert_eq!(app.world().resource::<LoadBrowser>().target(), 1);

        // Digits jump: 3, then Enter writes slot 03 and returns to the
        // pause panel (still paused).
        tap(&mut app, KeyCode::Digit3);
        assert_eq!(app.world().resource::<LoadBrowser>().target(), 3);
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Paused);

        // The written slot carries the running world's seed.
        let data = crate::plugins::save::read_save(3).expect("slot 03 was written");
        assert_eq!(data.seed, app.world().resource::<WorldSeed>().0);
        let _ = std::fs::remove_file(save_path(3));
    }

    #[test]
    fn pause_save_esc_returns_to_the_pause_panel() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);
        tap(&mut app, PAUSE);
        tap(&mut app, KeyCode::ArrowDown);
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::SavePick);

        // Esc backs out one level: the pause panel, still paused — the same
        // press must not also trigger the resume shortcut.
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Paused);
    }

    #[test]
    fn loading_then_back_to_menu_lands_on_the_root_menu() {
        let _guard = SAVES_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);

        // Park a save in slot 5 and load it through the browser.
        let mut bitmap = ExploredBitmap::default();
        bitmap.mark((1, 1));
        let data = crate::plugins::save::SaveData::capture(
            0xFEED,
            crate::plugins::save::Season::Summer,
            &bitmap,
            [10.0, 20.0],
            0.0,
        );
        write_save(5, &data).expect("writes the parking save");
        pick(&mut app, 3);
        tap(&mut app, KeyCode::Digit5);
        tap(&mut app, KeyCode::Enter);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Playing);

        // Pausing must normalize away the stale load browser.
        tap(&mut app, PAUSE);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::Paused);

        // Back to the main menu: it must list the root rows, not the picker.
        tap(&mut app, KeyCode::Backspace);
        assert_eq!(*app.world().resource::<GamePhase>(), GamePhase::MainMenu);
        assert_eq!(*app.world().resource::<MenuScreen>(), MenuScreen::Root);
        let mut query = app.world_mut().query::<&Text>();
        let lines: Vec<String> = query
            .iter(app.world())
            .map(|text| text.to_string())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("Seed:")),
            "the root rows must be listed, got: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("Slot")),
            "the slot picker must not resurface"
        );
        let _ = std::fs::remove_file(save_path(5));
    }

    #[test]
    fn the_selected_row_is_marked_with_a_cursor() {
        let mut app = app_with_ui();
        app.update();
        app.update();
        past_title(&mut app);

        let mut query = app.world_mut().query::<&Text>();
        let lines: Vec<String> = query
            .iter(app.world())
            .map(|text| text.to_string())
            .collect();
        let marked: Vec<&String> = lines.iter().filter(|line| line.starts_with("> ")).collect();
        assert_eq!(marked.len(), 1, "exactly one row carries the cursor");
        assert!(
            marked[0].contains("Seed"),
            "the cursor starts on the first row (Seed), got: {}",
            marked[0]
        );

        // Up wraps to the last row, which is Quit.
        tap(&mut app, KeyCode::ArrowUp);
        app.update();

        let mut query = app.world_mut().query::<&Text>();
        let lines: Vec<String> = query
            .iter(app.world())
            .map(|text| text.to_string())
            .collect();
        let marked: Vec<&String> = lines.iter().filter(|line| line.starts_with("> ")).collect();
        assert_eq!(marked.len(), 1);
        assert!(
            marked[0].contains("Quit"),
            "ArrowUp must wrap to the last row (Quit), got: {}",
            marked[0]
        );
    }

    #[test]
    fn hud_denominator_is_the_live_chunk_grid() {
        // The HUD used to hard-code 1600 (the old 40x40 map) while the world
        // is GRID_CELLS x GRID_CELLS; a full walk showed "256 / 1600".
        let mut app = app_with_ui();
        app.update();
        app.update();
        start_a_run(&mut app);

        let total = (crate::world::GRID_CELLS * crate::world::GRID_CELLS) as usize;
        let mut query = app.world_mut().query::<&Text>();
        let hud = query
            .iter(app.world())
            .find(|text| text.starts_with("Explored:"))
            .map(|text| (**text).clone())
            .expect("the playing HUD shows exploration progress");
        assert!(
            hud.contains(&format!("/ {total} ")),
            "HUD must use the live grid size, got: {hud}"
        );
    }
}
