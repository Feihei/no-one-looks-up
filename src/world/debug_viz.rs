//! Debug visualization (R2.7): chunk boundaries, street network, and block
//! labels drawn with Bevy Gizmos in development mode.
//!
//! Toggled with [`crate::core::input::DEV_TOGGLE_GIZMOS`] (G). Off by
//! default in all builds (user asked: no boundary lines until you press G);
//! [`examples/top_view.rs`] and [`tests/top_view.rs`] force their own gizmo
//! capture. The label entities are
//! plain `Text2d` markers; the gizmo drawing is immediate-mode per frame.

use bevy::prelude::*;

use crate::core::input::DEV_TOGGLE_GIZMOS;
use crate::world::CHUNK_SIZE;
use crate::world::chunks::{ChunkManager, WorldSystems};
use crate::world::collision::{CollisionIndex, PrismKind};
use crate::world::layout_graph::{BlockType, LayoutGraph};

/// Whether debug visualization is on (toggled with G).
///
/// `bool`'s `Default` is `false`, which is the required off-until-G state (the
/// toggle still works everywhere, and the top_view examples/tests force their
/// own gizmo capture).
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct DebugVizEnabled(pub bool);

/// Marker on block label entities.
#[derive(Component, Debug)]
pub struct BlockLabel;

/// Flips the debug visualization flag and syncs the label visibility.
pub(crate) fn toggle_debug_viz(
    input: Res<ButtonInput<KeyCode>>,
    mut enabled: ResMut<DebugVizEnabled>,
    mut labels: Query<&mut Visibility, With<BlockLabel>>,
) {
    if input.just_pressed(DEV_TOGGLE_GIZMOS) {
        enabled.0 = !enabled.0;
        info!(
            "debug visualization {}",
            if enabled.0 { "on" } else { "off" }
        );
        for mut visibility in labels.iter_mut() {
            *visibility = if enabled.0 {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
        }
    }
}

/// Spawns one text label per block the first time the layout graph appears.
pub(crate) fn spawn_block_labels(graph: Option<Res<LayoutGraph>>, mut commands: Commands) {
    let Some(graph) = graph else {
        return;
    };
    if !graph.is_changed() {
        return;
    }
    for block in &graph.blocks {
        let center = block.center();
        commands.spawn((
            Name::new(format!("BlockLabel({})", block.id)),
            BlockLabel,
            Text2d::new(format!("{:?}", block.kind)),
            TextFont {
                font_size: FontSize::Px(6.0),
                ..default()
            },
            Visibility::Hidden,
            Transform::from_xyz(center.x, 2.0, center.y),
        ));
    }
    info!("debug labels spawned: {} blocks", graph.blocks.len());
}

/// Draws the debug overlay: chunk boundaries, street network, block outlines.
pub(crate) fn draw_debug_viz(
    enabled: Res<DebugVizEnabled>,
    manager: Option<Res<ChunkManager>>,
    graph: Option<Res<LayoutGraph>>,
    collision: Option<Res<CollisionIndex>>,
    mut gizmos: Gizmos,
) {
    if !enabled.0 {
        return;
    }

    // Chunk boundaries (active set only).
    if let Some(manager) = &manager {
        for key in manager.active.keys() {
            let (x, z) = *key;
            let min = Vec3::new(x as f32 * CHUNK_SIZE, 0.05, z as f32 * CHUNK_SIZE);
            let step = Vec3::new(CHUNK_SIZE, 0.0, 0.0);
            let depth = Vec3::new(0.0, 0.0, CHUNK_SIZE);
            let c = Color::srgba(1.0, 1.0, 1.0, 0.25);
            gizmos.linestrip([min, min + step, min + step + depth, min + depth, min], c);
        }
    }

    // Street network segments (centerlines). ADR-0008: lanes draw in their
    // own cooler colour so the two road tiers read apart in the overlay.
    // ADR-0009: the arterial cross draws hot pink-white, clearly above the
    // ordinary street orange.
    if let Some(graph) = &graph {
        let street = Color::srgba(1.0, 0.55, 0.1, 0.8);
        let arterial = Color::srgba(1.0, 0.3, 0.75, 0.9);
        let lane = Color::srgba(0.4, 0.8, 1.0, 0.7);
        for edge in &graph.edges {
            let (Some(a), Some(b)) = (
                graph.nodes.get(edge.from as usize),
                graph.nodes.get(edge.to as usize),
            ) else {
                continue;
            };
            let color = match edge.kind {
                crate::world::layout_graph::EdgeKind::Street => street,
                crate::world::layout_graph::EdgeKind::Arterial => arterial,
                crate::world::layout_graph::EdgeKind::Lane => lane,
            };
            gizmos.line(
                Vec3::new(a.pos.x, 0.1, a.pos.y),
                Vec3::new(b.pos.x - a.pos.x, 0.0, b.pos.y - a.pos.y),
                color,
            );
        }

        // Block outlines (inset polygons), colored by type.
        for block in &graph.blocks {
            if block.polygon.len() < 3 {
                continue;
            }
            let color = match block.kind {
                BlockType::OldCity => Color::srgba(0.9, 0.3, 0.3, 0.6),
                BlockType::Apartment => Color::srgba(0.3, 0.5, 0.9, 0.6),
                BlockType::Factory => Color::srgba(0.9, 0.9, 0.3, 0.6),
                BlockType::Plaza => Color::srgba(0.3, 0.9, 0.3, 0.6),
                BlockType::Forest => Color::srgba(0.2, 0.7, 0.3, 0.6),
                // 城中村: burnt orange, distinct from the old city's red.
                BlockType::UrbanVillage => Color::srgba(0.95, 0.55, 0.2, 0.6),
            };
            let mut points: Vec<Vec3> = block
                .polygon
                .iter()
                .map(|p| Vec3::new(p.x, 0.08, p.y))
                .collect();
            points.push(points[0]);
            gizmos.linestrip(points, color);

            // ADR-0008: sub-block plots, a shade dimmer than the parent
            // outline so the lane grid reads inside the block.
            for child in &block.children {
                if child.polygon.len() < 3 {
                    continue;
                }
                let mut plot: Vec<Vec3> = child
                    .polygon
                    .iter()
                    .map(|p| Vec3::new(p.x, 0.09, p.y))
                    .collect();
                plot.push(plot[0]);
                gizmos.linestrip(plot, Color::srgba(0.85, 0.85, 0.9, 0.45));
            }
        }
    }

    // Solid prisms (R3.3): what actually blocks the walker. Drawn just above
    // the block outlines so a walk test reads "this is where I bounce off".
    if let (Some(manager), Some(collision)) = (&manager, &collision) {
        for key in manager.active.keys() {
            for prism in collision.prisms_in(*key) {
                if prism.polygon.len() < 3 {
                    continue;
                }
                let color = match prism.kind {
                    PrismKind::OldCity => Color::srgba(1.0, 0.25, 0.25, 0.9),
                    PrismKind::Apartment => Color::srgba(0.3, 0.6, 1.0, 0.9),
                    PrismKind::Factory => Color::srgba(1.0, 0.9, 0.2, 0.9),
                    PrismKind::UrbanVillage => Color::srgba(1.0, 0.55, 0.15, 0.9),
                    PrismKind::SceneryRing => Color::srgba(0.6, 0.6, 0.65, 0.55),
                };
                let mut points: Vec<Vec3> = prism
                    .polygon
                    .iter()
                    .map(|p| Vec3::new(p.x, prism.height, p.y))
                    .collect();
                points.push(points[0]);
                gizmos.linestrip(points, color);
            }
        }
    }
}

/// Debug visualization plugin: gizmo overlay + block labels, toggled with G.
///
/// Ordering contract (R2.8): explicit chain
/// `spawn_block_labels -> toggle_debug_viz -> draw_debug_viz` (label spawn
/// writes visibility the toggle reads; toggle writes the flag draw reads),
/// and `WorldSystems::Streaming` before drawing (draw reads the chunk set).
pub struct DebugVizPlugin;

impl Plugin for DebugVizPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DebugVizEnabled>().add_systems(
            Update,
            (
                spawn_block_labels,
                toggle_debug_viz.after(spawn_block_labels),
                draw_debug_viz
                    .after(toggle_debug_viz)
                    .after(WorldSystems::Streaming),
            ),
        );
    }
}
