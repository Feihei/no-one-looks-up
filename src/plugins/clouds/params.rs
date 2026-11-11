//! GPU uniform of every cloud kernel (`common.wesl`, `CloudParams`).
//!
//! Every field is a vec4 so the Rust and WESL layouts agree without padding
//! logic. `params_mirror` in [`crate::plugins::clouds`] pins the mirror.

use bevy::math::{UVec4, Vec4};
use bevy::render::render_resource::ShaderType;

/// The uniform the march, resolve and composite kernels read.
#[derive(Clone, Copy, Debug, Default, ShaderType)]
pub struct CloudsParams {
    /// xyz: camera world position, m. w: planet radius, m.
    pub camera: Vec4,
    /// x: atmosphere outer radius, m. y: aerial-view LUT range, m. z: aerial-view LUT slices.
    /// w: the night sky's background radiance, cd/m².
    pub atmo: Vec4,
    /// x: layer base altitude, m. y: layer top altitude, m. z: cloud type 0–1. w: cover 0–1.
    pub layer: Vec4,
    /// xyz: unit direction toward the sun. w: unused.
    pub light: Vec4,
    /// xyz: the sun's illuminance outside the atmosphere, lx. w: ambient source, cd/m².
    pub light_e: Vec4,
    /// xy: drift of the weather field, m (C1). zw: drift of the base noise, m (C1).
    pub drift: Vec4,
    /// xy: drift of the detail noise, m (C1). zw: unit wind direction (C1).
    pub drift_detail: Vec4,
    /// xy: weather map origin, m (C1). z: weather map extent, m (C1). w: unused.
    pub weather: Vec4,
    /// x: frame counter. y: primary march steps. z: light samples. w: unused.
    pub counts: UVec4,
    /// xy: full resolution. zw: march resolution.
    pub sizes: UVec4,
    /// x: debug mode (0 off, 1 uniform shell). y: debug shell luminance. z: unused.
    /// w: 1 when the history is invalid and the resolve must write the fresh march.
    pub misc: Vec4,
    /// x: world units per metre ([`bevy_elements::ElementsUnits`]). The shaders divide what
    /// they take from the view matrices and depth by this and multiply what they hand back.
    pub units: Vec4,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_one_vec4_of_the_same_width() {
        // The WESL mirror relies on every field being 16 bytes: all-vec4
        // structs need no alignment or padding logic, in either language.
        assert_eq!(size_of::<CloudsParams>(), 16 * 12);
    }
}
