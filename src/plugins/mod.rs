//! Feature plugins for No One Looks Up Even Alone.
//!
//! Plugin split mirrors the original Godot Manager layout (see ADR-0005
//! mapping table): SkyPlugin, WorldPlugin, SeasonPlugin, SavePlugin,
//! MenuPlugin. Each milestone fills in the corresponding plugin.

pub mod cirrus;
pub mod clouds;
pub mod facade;
pub mod hills;
pub mod menu;
pub mod minimap;
pub mod save;
pub mod save_data;
pub mod season;
pub mod sky;
pub mod sky_panel;
// R3.9: the F7 tree gallery is gone. It was a one-shot snapshot pinned where
// the player stood, so walking away left it behind and it needed two F7 presses
// to rebuild — which read as "trees don't stream with chunks". World trees now
// come from chunk content (`world::chunks` + `world::tree_mesh`), and the
// silhouette study lives in `examples/tree_gallery.rs`.
pub mod world;

pub use sky::GoldenHourCamera;

use bevy::app::PluginGroupBuilder;
use bevy::prelude::*;

use cirrus::CirrusPlugin;
use clouds::CloudsPlugin;
use facade::FacadePlugin;
use hills::HillsPlugin;
use menu::MenuPlugin;
use minimap::MinimapPlugin;
use save::SavePlugin;
use season::SeasonPlugin;
use sky::SkyPlugin;
use sky_panel::SkyPanelPlugin;
use world::WorldPlugin;

use crate::world::debug_viz::DebugVizPlugin;

/// Group plugin wiring every feature plugin in dependency order.
pub struct GamePlugins;

impl PluginGroup for GamePlugins {
    fn build(self) -> PluginGroupBuilder {
        PluginGroupBuilder::start::<Self>()
            .add(SkyPlugin)
            // The game's volumetric clouds replace the crate's disabled
            // CloudPlugin (adr-0007); right after the sky so its depth
            // prepass observer is registered before any camera spawns.
            .add(CloudsPlugin)
            .add(SkyPanelPlugin)
            .add(CirrusPlugin)
            .add(HillsPlugin)
            .add(WorldPlugin)
            .add(DebugVizPlugin)
            // NOTE (R3.9): the F7 gallery is NOT registered. It was a
            // one-shot snapshot pinned where the player stood, so walking away
            // left it behind and it needed two F7 presses to rebuild — which
            // read as "trees don't stream with chunks". World trees now come
            // from chunk content (`chunks.rs` + `world::tree_mesh`); the
            // gallery module stays as a shape reference for
            // `examples/tree_gallery.rs`.
            // Facade material type: registered before the world streams any
            // building, because the material only renders once its
            // `MaterialPlugin` exists.
            .add(FacadePlugin)
            .add(SeasonPlugin)
            .add(SavePlugin)
            .add(MenuPlugin)
            .add(MinimapPlugin)
    }
}
