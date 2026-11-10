//! Minimap HUD (R4.x): a small top-right map showing the street network the
//! player has explored, plus the player's position and facing.
//!
//! Rendered as plain `bevy_ui` nodes so it shares the existing HUD stack and
//! needs no extra camera or render layer. Streets are single-line segments
//! taken from the [`LayoutGraph`] edges; a segment is drawn only when its
//! midpoint chunk appears in the [`ExploredBitmap`], so unexplored parts of
//! the city stay blank (the discovery feedback ADR-0001 asks for).
//!
//! Toggled with `M` while walking (see [`crate::core::input::MINIMAP_TOGGLE`]);
//! shown by default while playing, hidden in every non-playing phase.

use bevy::input::InputSystems;
use bevy::math::Rot2;
use bevy::prelude::*;

use crate::core::game_state::GamePhase;
use crate::core::input::MINIMAP_TOGGLE;
use crate::player::PlayerBody;
use crate::world::WORLD_SIZE;
use crate::world::collision::chunk_key;
use crate::world::explored::ExploredBitmap;
use crate::world::layout_graph::LayoutGraph;

/// Side of the minimap square, in logical pixels.
const MAP_SIZE: f32 = 240.0;
/// Pixels per world metre (`MAP_SIZE / WORLD_SIZE`, 0.3 at the 800 m map).
const PIXELS_PER_METER: f32 = MAP_SIZE / WORLD_SIZE;
/// Street line thickness, in pixels.
const ROAD_THICKNESS: f32 = 1.5;
/// Player marker line thickness, in pixels.
const MARKER_THICKNESS: f32 = 2.0;

const MAP_BACKGROUND: Color = Color::srgba(0.03, 0.04, 0.06, 0.62);
const MAP_BORDER: Color = Color::srgba(0.85, 0.78, 0.62, 0.55);
const ROAD_COLOR: Color = Color::srgb(0.80, 0.77, 0.67);
const PLAYER_COLOR: Color = Color::srgb(1.0, 0.72, 0.25);

/// Player arrow in local map pixels: a shaft and two barbs pointing up (-y).
/// Local up maps to the body's facing when the arrow is rotated by `-yaw`
/// (Bevy's forward is -Z at yaw 0, and the map has +Z pointing down).
const PLAYER_ARROW: [(Vec2, Vec2); 3] = [
    (Vec2::new(0.0, 0.0), Vec2::new(0.0, -10.0)),
    (Vec2::new(0.0, -10.0), Vec2::new(-4.0, -6.0)),
    (Vec2::new(0.0, -10.0), Vec2::new(4.0, -6.0)),
];

pub struct MinimapPlugin;

impl Plugin for MinimapPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MinimapVisible>()
            .init_resource::<MinimapUiRoot>()
            .add_systems(
                PreUpdate,
                // Input systems clear + replay `ButtonInput` every PreUpdate;
                // reading it before that would miss or double-count the M
                // press depending on how Bevy resolves the ambiguity.
                toggle_minimap.after(InputSystems),
            )
            .add_systems(
                Update,
                (sync_minimap_root, rebuild_roads, update_player_marker).chain(),
            );
    }
}

/// Whether the minimap is shown (toggled with `M` while playing).
#[derive(Resource, Debug, Clone, Copy)]
struct MinimapVisible(bool);

impl Default for MinimapVisible {
    fn default() -> Self {
        // A HUD is information you should not have to ask for: visible from
        // the first step, `M` hides it.
        Self(true)
    }
}

/// Root entity of the minimap UI, plus the layer roads are spawned under.
#[derive(Resource, Debug, Default)]
struct MinimapUiRoot {
    entity: Option<Entity>,
    /// Dedicated child layer so newly rebuilt roads still draw beneath the
    /// player marker (UI order follows sibling order, not spawn time).
    roads_layer: Option<Entity>,
    /// Explored-chunk count the current roads were built for; `usize::MAX`
    /// forces a rebuild right after the root appears.
    last_explored: usize,
}

/// A single street line currently drawn on the map.
#[derive(Component)]
struct MinimapRoad;

/// One line of the player's direction arrow.
#[derive(Component)]
struct MinimapPlayerSegment {
    from: Vec2,
    to: Vec2,
}

/// `M` toggles the minimap, but only while walking (`Playing`).
fn toggle_minimap(
    phase: Res<GamePhase>,
    keys: Res<ButtonInput<KeyCode>>,
    mut visible: ResMut<MinimapVisible>,
) {
    if phase.is_game_active() && keys.just_pressed(MINIMAP_TOGGLE) {
        visible.0 = !visible.0;
    }
}

/// Spawns/despawns the minimap root as the phase or toggle changes.
fn sync_minimap_root(
    phase: Res<GamePhase>,
    visible: Res<MinimapVisible>,
    mut root: ResMut<MinimapUiRoot>,
    mut commands: Commands,
) {
    let wanted = phase.is_game_active() && visible.0;
    if wanted && root.entity.is_none() {
        let mut layer = None;
        let entity = commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(12.0),
                    right: Val::Px(14.0),
                    width: Val::Px(MAP_SIZE),
                    height: Val::Px(MAP_SIZE),
                    ..default()
                },
                BackgroundColor(MAP_BACKGROUND),
                Outline {
                    width: Val::Px(1.0),
                    offset: Val::Px(0.0),
                    color: MAP_BORDER,
                },
                GlobalZIndex(1),
            ))
            .with_children(|parent| {
                let roads = parent
                    .spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            left: Val::Px(0.0),
                            top: Val::Px(0.0),
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            ..default()
                        },
                        BackgroundColor(Color::NONE),
                    ))
                    .id();
                layer = Some(roads);
                for (from, to) in PLAYER_ARROW {
                    parent.spawn((
                        Node {
                            position_type: PositionType::Absolute,
                            width: Val::Px((to - from).length().max(0.5)),
                            height: Val::Px(MARKER_THICKNESS),
                            ..default()
                        },
                        UiTransform::default(),
                        BackgroundColor(PLAYER_COLOR),
                        MinimapPlayerSegment { from, to },
                    ));
                }
            })
            .id();
        root.entity = Some(entity);
        root.roads_layer = layer;
        root.last_explored = usize::MAX;
    } else if !wanted && root.entity.is_some() {
        if let Some(entity) = root.entity.take() {
            commands.entity(entity).try_despawn();
        }
        root.roads_layer = None;
    }
}

/// Rebuilds the street lines whenever the explored set grows.
///
/// Roads are filtered by chunk: only edges whose midpoint chunk has been
/// explored appear. Rebuilding wholesale is cheap enough (a few hundred
/// nodes) and happens only when a new chunk is discovered.
fn rebuild_roads(
    phase: Res<GamePhase>,
    visible: Res<MinimapVisible>,
    graph: Option<Res<LayoutGraph>>,
    bitmap: Res<ExploredBitmap>,
    mut root: ResMut<MinimapUiRoot>,
    mut commands: Commands,
    roads: Query<Entity, With<MinimapRoad>>,
) {
    if !phase.is_game_active() || !visible.0 {
        return;
    }
    let (Some(layer), Some(graph)) = (root.roads_layer, graph) else {
        return;
    };
    // A new world (new seed / restarted run) replaces the graph resource, so
    // rebuild even when the explored count happens to match.
    if bitmap.len() == root.last_explored && !graph.is_changed() {
        return;
    }
    root.last_explored = bitmap.len();

    for entity in &roads {
        commands.entity(entity).try_despawn();
    }
    let segments = explored_segments(&graph, &bitmap);
    commands.entity(layer).with_children(|parent| {
        for (from, to) in segments {
            parent.spawn(line_node(
                world_to_map(from),
                world_to_map(to),
                ROAD_THICKNESS,
                ROAD_COLOR,
                MinimapRoad,
            ));
        }
    });
}

/// Moves and rotates the player arrow to the current pose.
fn update_player_marker(
    phase: Res<GamePhase>,
    visible: Res<MinimapVisible>,
    root: Res<MinimapUiRoot>,
    players: Query<(&Transform, Option<&PlayerBody>), With<Camera3d>>,
    mut segments: Query<(&MinimapPlayerSegment, &mut Node, &mut UiTransform)>,
) {
    if !phase.is_game_active() || !visible.0 || root.entity.is_none() {
        return;
    }
    let Ok((transform, body)) = players.single() else {
        return;
    };
    let center = world_to_map(Vec2::new(transform.translation.x, transform.translation.z));
    let yaw = body.map(|body| body.yaw).unwrap_or(0.0);
    let angle = -yaw;

    for (segment, mut node, mut ui) in &mut segments {
        let from = rotate(segment.from, angle) + center;
        let to = rotate(segment.to, angle) + center;
        let delta = to - from;
        let length = delta.length();
        let mid = (from + to) * 0.5;
        node.left = Val::Px(mid.x - length * 0.5);
        node.top = Val::Px(mid.y - MARKER_THICKNESS * 0.5);
        ui.rotation = Rot2::radians(delta.y.atan2(delta.x));
    }
}

/// Street segments whose midpoint chunk has been explored.
fn explored_segments(graph: &LayoutGraph, bitmap: &ExploredBitmap) -> Vec<(Vec2, Vec2)> {
    graph
        .edges
        .iter()
        .filter_map(|edge| {
            let from = graph.nodes.get(edge.from as usize)?;
            let to = graph.nodes.get(edge.to as usize)?;
            let mid = (from.pos + to.pos) * 0.5;
            bitmap
                .is_explored(chunk_key(mid))
                .then_some((from.pos, to.pos))
        })
        .collect()
}

/// World XZ metres to minimap pixels (top-left origin, +Z down like the map).
fn world_to_map(position: Vec2) -> Vec2 {
    position * PIXELS_PER_METER
}

/// Rotates `p` by `angle` in screen coordinates (y grows downwards).
fn rotate(p: Vec2, angle: f32) -> Vec2 {
    let (sin, cos) = angle.sin_cos();
    Vec2::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos)
}

/// A rotated single-line UI node from `from` to `to` (map pixels).
fn line_node(
    from: Vec2,
    to: Vec2,
    thickness: f32,
    color: Color,
    marker: impl Component,
) -> impl Bundle {
    let delta = to - from;
    let length = delta.length().max(0.5);
    let mid = (from + to) * 0.5;
    (
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(mid.x - length * 0.5),
            top: Val::Px(mid.y - thickness * 0.5),
            width: Val::Px(length),
            height: Val::Px(thickness),
            ..default()
        },
        UiTransform::from_rotation(Rot2::radians(delta.y.atan2(delta.x))),
        BackgroundColor(color),
        marker,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::layout_graph::{LayoutEdge, LayoutNode, NodeKind};
    use bevy::MinimalPlugins;

    fn minimap_app(phase: GamePhase) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, MinimapPlugin))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ExploredBitmap>()
            .insert_resource(phase);
        app.world_mut().spawn((
            Camera3d::default(),
            PlayerBody::default(),
            Transform::from_xyz(
                crate::world::world_center(),
                1.65,
                crate::world::world_center(),
            ),
        ));
        app
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        // `reset_all` (not `clear`) also releases the key: `press` only
        // records `just_pressed` when the key is not already held, so a
        // second toggle needs a real release in between.
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        app.update();
    }

    #[test]
    fn m_toggles_the_minimap_while_playing() {
        let mut app = minimap_app(GamePhase::Playing);
        app.update();
        assert!(
            app.world().resource::<MinimapVisible>().0,
            "a HUD is visible without being asked for"
        );
        assert!(
            app.world().resource::<MinimapUiRoot>().entity.is_some(),
            "playing spawns the minimap right away"
        );

        press(&mut app, MINIMAP_TOGGLE);
        assert!(!app.world().resource::<MinimapVisible>().0);
        assert!(
            app.world().resource::<MinimapUiRoot>().entity.is_none(),
            "M hides the minimap"
        );

        press(&mut app, MINIMAP_TOGGLE);
        assert!(app.world().resource::<MinimapVisible>().0);
        assert!(app.world().resource::<MinimapUiRoot>().entity.is_some());
    }

    #[test]
    fn leaving_the_playing_phase_hides_the_minimap() {
        let mut app = minimap_app(GamePhase::Playing);
        app.update();
        assert!(app.world().resource::<MinimapUiRoot>().entity.is_some());

        *app.world_mut().resource_mut::<GamePhase>() = GamePhase::Paused;
        app.update();
        assert!(app.world().resource::<MinimapUiRoot>().entity.is_none());
        assert!(
            app.world().resource::<MinimapVisible>().0,
            "the toggle stays on; resuming brings the map back"
        );

        *app.world_mut().resource_mut::<GamePhase>() = GamePhase::Playing;
        app.update();
        assert!(app.world().resource::<MinimapUiRoot>().entity.is_some());
    }

    #[test]
    fn roads_only_appear_on_explored_chunks() {
        let graph = LayoutGraph {
            nodes: vec![
                LayoutNode {
                    pos: Vec2::new(100.0, 100.0),
                    kind: NodeKind::Junction,
                },
                LayoutNode {
                    pos: Vec2::new(150.0, 100.0),
                    kind: NodeKind::Junction,
                },
            ],
            edges: vec![LayoutEdge {
                from: 0,
                to: 1,
                width: 8.0,
                left_block: None,
                right_block: None,
            }],
            ..Default::default()
        };

        let blank = ExploredBitmap::default();
        assert!(explored_segments(&graph, &blank).is_empty());

        let mut explored = ExploredBitmap::default();
        explored.mark(chunk_key(Vec2::new(125.0, 100.0)));
        let segments = explored_segments(&graph, &explored);
        assert_eq!(segments.len(), 1, "the explored street must be drawn");
        assert_eq!(segments[0].0, Vec2::new(100.0, 100.0));
        assert_eq!(segments[0].1, Vec2::new(150.0, 100.0));
    }

    #[test]
    fn map_projection_uses_the_world_extent() {
        assert_eq!(world_to_map(Vec2::ZERO), Vec2::ZERO);
        let corner = world_to_map(Vec2::splat(WORLD_SIZE));
        assert!((corner.x - MAP_SIZE).abs() < 1e-3, "{corner:?}");
        assert!((corner.y - MAP_SIZE).abs() < 1e-3, "{corner:?}");
    }

    #[test]
    fn player_arrow_rotates_up_to_the_body_facing() {
        // Up (-y) must become the body forward (-sin yaw, -cos yaw).
        for yaw in [0.0, std::f32::consts::FRAC_PI_2, std::f32::consts::PI] {
            let rotated = rotate(Vec2::new(0.0, -1.0), -yaw);
            let forward = Vec2::new(-yaw.sin(), -yaw.cos());
            assert!(
                (rotated - forward).length() < 1e-5,
                "yaw {yaw}: {rotated:?} vs {forward:?}"
            );
        }
    }
}
