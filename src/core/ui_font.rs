//! The one typeface every on-screen string uses.
//!
//! The ink typewriter face (`assets/fonts/ZhaohuaTypeWriterLight.ttf`, the
//! same ink the handwritten title is cut from) is loaded once here and handed
//! to each text spawn through [`UiFont`], so a menu line cannot quietly drift
//! back onto Bevy's built-in font. Headless tests have no `AssetServer`: the
//! handle stays `None` and `TextFont` falls back to the default font, which
//! keeps the menu flow testable without a GPU or an asset folder.

use bevy::prelude::*;
use bevy::text::{FontSource, TextFont};

/// Asset path of the UI face. Kept next to [`UiFont`] so the loader and
/// `scripts/package.sh`'s verify list cannot drift apart.
pub const UI_FONT_PATH: &str = "fonts/ZhaohuaTypeWriterLight.ttf";

/// Handle to the UI font; `None` when there is no asset server (headless
/// tests), in which case `TextFont` keeps the built-in face.
#[derive(Resource, Debug, Default, Clone)]
pub struct UiFont(pub Option<Handle<Font>>);

impl UiFont {
    /// The face list to hand a `TextFont`: the ink font first, Bevy's built-in
    /// face as the fallback for anything it lacks. Without an asset server
    /// (`None`) this is just the default source.
    pub fn source(&self) -> FontSource {
        match &self.0 {
            Some(handle) => FontSource::List(vec![
                FontSource::Handle(handle.clone()),
                FontSource::default(),
            ]),
            None => FontSource::default(),
        }
    }

    /// A `TextFont` of `size` px in the UI face.
    pub fn at(&self, size: f32) -> TextFont {
        TextFont {
            font: self.source(),
            ..TextFont::from_font_size(size)
        }
    }
}

/// Loads [`UI_FONT_PATH`] at startup and installs [`UiFont`].
pub struct UiFontPlugin;

impl Plugin for UiFontPlugin {
    fn build(&self, app: &mut App) {
        let handle = app
            .world()
            .get_resource::<AssetServer>()
            .map(|server| server.load(UI_FONT_PATH));
        app.insert_resource(UiFont(handle));
    }
}
