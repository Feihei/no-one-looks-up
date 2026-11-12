//! GPU uniform of every cloud kernel (`common.wesl`, `CloudParams`).
//!
//! Every field is a vec4 so the Rust and WESL layouts agree without padding
//! logic. `params_mirror_wesl_struct` in [`crate::plugins::clouds`] pins the
//! mirror.

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
    /// xyz: the sun's illuminance outside the atmosphere, lx. w: unused (C2's ambient source).
    pub light_e: Vec4,
    /// xy: drift of the weather field, m (wrapped to its tile). zw: drift of the base noise, m.
    pub drift: Vec4,
    /// xy: drift of the detail noise, m. zw: unit wind direction (XZ).
    pub drift_detail: Vec4,
    /// xy: weather map origin, m. z: weather map extent, m. w: unused.
    pub weather: Vec4,
    /// x: frame counter. y: primary march steps. z: light samples. w: first z slice of this
    /// frame's noise slab.
    pub counts: UVec4,
    /// xy: full resolution. zw: march resolution.
    pub sizes: UVec4,
    /// x: debug mode (0 off, 1 uniform shell). y: debug shell luminance. z: unused.
    /// w: 1 when the history is invalid and the resolve must write the fresh march.
    pub misc: Vec4,
    /// x: world units per metre ([`bevy_elements::ElementsUnits`]). The shaders divide what
    /// they take from the view matrices and depth by this and multiply what they hand back.
    pub units: Vec4,
    /// The height gradient quadruple (a0, a1, a2, a3) of the selected kind.
    pub grad: Vec4,
    /// x: peak extinction, m⁻¹. y: cloud-side erosion strength. z: weight of the dome shaping
    /// (0 a flat deck, 1 cumulus). w: unused.
    pub shape: Vec4,
    /// x: base noise tile, m. y: detail noise tile, m. z: wind skew per unit height, m.
    /// w: unused.
    pub noise: Vec4,
    /// x: ambient brightness fuse (1 neutral). y: aerial in-scatter fuse (1 neutral).
    /// z: powder strength (1 neutral). w: multiple-scattering strength (1 neutral).
    /// The two fuses exist to retune the cloud against the sky by eye without a rebuild
    /// (adr-0007); they start at 1.
    pub tuning: Vec4,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_is_one_vec4_of_the_same_width() {
        // The WESL mirror relies on every field being 16 bytes: all-vec4
        // structs need no alignment or padding logic, in either language.
        assert_eq!(size_of::<CloudsParams>(), 16 * 16);
    }
}
