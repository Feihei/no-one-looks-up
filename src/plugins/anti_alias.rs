//! Temporal anti-aliasing for the walker's camera (R3.16).
//!
//! The renderer runs `Msaa::Off` for a hard reason: the cloud composite reads a
//! single-sampled depth-only prepass target (`plugins::clouds::gpu`, which bails
//! out on anything but `Msaa::Off`). So every silhouette in the game -- a
//! roofline against the sky, a facade's plinth line, a 2 cm catenary wire -- is
//! resolved with one sample per pixel. Walking turns that into crawling edges,
//! and wires past ~60 m flicker as they cross pixel boundaries.
//!
//! TAA is the only antialiasing that fixes the second half as well. A line
//! thinner than a pixel is not a sampling problem MSAA can help with -- there
//! is nothing to average *within* the pixel -- but it is exactly what temporal
//! accumulation solves: over a dozen jittered frames the wire covers some of
//! the pixel some of the time, and the blend settles on a dim continuous line.
//!
//! ## What it costs
//!
//! TAA blends the current frame with the history, so anything that moves
//! without a motion vector leaves a trail. The world is static and the camera
//! is the only mover, so geometry is covered by the view's motion vectors --
//! but the sky and the clouds are written into the same target by fullscreen
//! passes, and turning the camera drags their edges for a frame or two. Set
//! `HU_TAA=off` (or `0`) to compare without a rebuild; everything else about
//! the renderer is unchanged, since `DepthPrepass` and `Msaa::Off` already
//! satisfy TAA's requirements.
//!
//! The frame cost is the motion-vector prepass, which re-rasterises every mesh
//! in view. Measured with `examples/view_shot` at 1280x720 over the ring road
//! (30-frame window, debug build with `opt-level = 1`): 19-22 ms without, 26-27
//! ms with -- about 6 ms, or a fifth of the frame. In motion the frame is
//! steady up to roughly 3 px of displacement per frame (a 10 deg/s turn
//! horizontally, faster vertically); past that the leaf cards of a tree start
//! to trail, because a tree is thousands of sub-pixel edges and TAA's
//! neighbourhood clamp has nothing stable to lock onto.

use bevy::anti_alias::taa::TemporalAntiAliasing;
use bevy::prelude::*;
use bevy_elements::ElementsCamera;

/// Adds [`TemporalAntiAliasing`] to every camera that carries an
/// [`ElementsCamera`], unless `HU_TAA` says otherwise.
pub struct AntiAliasPlugin;

impl Plugin for AntiAliasPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(attach_taa);
    }
}

/// Observes the camera instead of reading a bundle field, so the example
/// binaries (`view_shot`, `top_view`) get the same treatment for free.
fn attach_taa(trigger: On<Add<ElementsCamera>>, mut commands: Commands) {
    if !enabled() {
        return;
    }
    // `reset: true` on the first frame discards the (empty) history and
    // samples the current frame clean, which is what the default is for.
    commands
        .entity(trigger.entity)
        .insert(TemporalAntiAliasing::default());
}

/// `HU_TAA=off` / `0` / `false` disables it; anything else, or no variable at
/// all, leaves it on.
fn enabled() -> bool {
    parse(std::env::var("HU_TAA").ok().as_deref())
}

/// Split from [`enabled`] because the process environment is global: a test
/// that set `HU_TAA` would race every other test in the binary.
fn parse(raw: Option<&str>) -> bool {
    match raw {
        Some(raw) => !matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no"
        ),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn taa_is_on_unless_the_switch_says_otherwise() {
        assert!(parse(None), "no setting must mean on");
        assert!(parse(Some("")), "an empty value is not a way to say off");
        assert!(parse(Some("on")));
        assert!(parse(Some("1")));
        for off in ["off", "0", "false", "no", " OFF ", "False"] {
            assert!(!parse(Some(off)), "{off:?} must disable");
        }
    }
}
