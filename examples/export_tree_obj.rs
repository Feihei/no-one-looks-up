//! Exports the baked procedural trees for inspection in Blender.
//!
//! Run with `cargo run --example export_tree_obj --release`.
//!
//! Writes `.tmp/trees/{tree_models.obj,tree_models.mtl,leaf_atlas.tga}` (the
//! preview models are scratch output and stay out of git):
//! - one object per species/variant, split into `<name>_wood` and
//!   `<name>_leaves` so the branch skeleton and the leaf cards can be toggled
//!   independently in Blender;
//! - a near-tier bake (every twig and every card), Y-up like Bevy;
//! - the same procedural leaf atlas the game bakes at runtime, so the card
//!   silhouettes are visible instead of opaque rectangles.

use std::fmt::Write as _;
use std::fs;
use std::io;

use bevy::mesh::{Indices, Mesh, VertexAttributeValues};
use no_one_looks_up::world::leaf_shape::{ATLAS_HEIGHT, AlphaMask, bake_species_atlas};
use no_one_looks_up::world::tree_mesh::{TreeMeshKey, bake_tree_mesh};

const OUT_DIR: &str = ".tmp/trees";

fn main() -> io::Result<()> {
    fs::create_dir_all(OUT_DIR)?;

    let atlas = bake_species_atlas(ATLAS_HEIGHT);
    write_tga(&format!("{OUT_DIR}/leaf_atlas.tga"), &atlas)?;

    let mut obj = String::new();
    obj.push_str(
        "# Procedural trees: No One Looks Up Even Alone\n\
         # Y-up, metres. Wood and leaves are separate objects per tree.\n\
         mtllib tree_models.mtl\n",
    );
    let mut counters = Counters::default();

    for species in 0..3u8 {
        for variant in 0..TreeMeshKey::VARIANTS {
            let parts = bake_tree_mesh(TreeMeshKey { species, variant });
            // Grid: variants along X, species along Z, so Blender's default
            // import shows all twelve side by side without overlap.
            let offset = [(variant as f32 - 1.5) * 10.0, 0.0, species as f32 * 12.0];
            let name = format!("s{species}_v{variant}");
            write_object(
                &mut obj,
                &format!("{name}_wood"),
                "wood",
                &parts.wood,
                offset,
                false,
                &mut counters,
            );
            write_object(
                &mut obj,
                &format!("{name}_leaves"),
                "leaves",
                &parts.leaves,
                offset,
                true,
                &mut counters,
            );
        }
    }

    fs::write(format!("{OUT_DIR}/tree_models.obj"), obj)?;
    fs::write(format!("{OUT_DIR}/tree_models.mtl"), MTL)?;
    println!(
        "wrote {OUT_DIR}/tree_models.obj ({} objects), tree_models.mtl, leaf_atlas.tga",
        3 * TreeMeshKey::VARIANTS as usize * 2
    );
    Ok(())
}

#[derive(Default)]
struct Counters {
    vertices: u32,
    uvs: u32,
    normals: u32,
}

/// One OBJ object. OBJ indices are global across the file, so `counters` carries
/// the offsets of every earlier object.
fn write_object(
    out: &mut String,
    name: &str,
    material: &str,
    mesh: &Mesh,
    offset: [f32; 3],
    with_uvs: bool,
    counters: &mut Counters,
) {
    let positions = match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(VertexAttributeValues::Float32x3(v)) => v.clone(),
        _ => Vec::new(),
    };
    if positions.is_empty() {
        return;
    }
    let vertex_normals = match mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
        Some(VertexAttributeValues::Float32x3(v)) => v.clone(),
        _ => vec![[0.0, 1.0, 0.0]; positions.len()],
    };
    let uv_data = match mesh.attribute(Mesh::ATTRIBUTE_UV_0) {
        Some(VertexAttributeValues::Float32x2(v)) => v.clone(),
        _ => Vec::new(),
    };
    let indices = match mesh.indices() {
        Some(Indices::U32(v)) => v.clone(),
        Some(Indices::U16(v)) => v.iter().map(|&i| i as u32).collect(),
        None => (0..positions.len() as u32).collect(),
    };

    let v0 = counters.vertices + 1;
    let vt0 = counters.uvs + 1;
    let vn0 = counters.normals + 1;

    let _ = writeln!(out, "o {name}");
    for p in &positions {
        let _ = writeln!(
            out,
            "v {:.5} {:.5} {:.5}",
            p[0] + offset[0],
            p[1] + offset[1],
            p[2] + offset[2]
        );
    }
    if with_uvs {
        for uv in &uv_data {
            let _ = writeln!(out, "vt {:.5} {:.5}", uv[0], 1.0 - uv[1]);
        }
    }
    for n in &vertex_normals {
        let _ = writeln!(out, "vn {:.5} {:.5} {:.5}", n[0], n[1], n[2]);
    }
    let _ = writeln!(out, "usemtl {material}");
    for tri in indices.chunks(3) {
        if tri.len() < 3 {
            continue;
        }
        if with_uvs && !uv_data.is_empty() {
            let _ = writeln!(
                out,
                "f {}/{}/{} {}/{}/{} {}/{}/{}",
                v0 + tri[0],
                vt0 + tri[0],
                vn0 + tri[0],
                v0 + tri[1],
                vt0 + tri[1],
                vn0 + tri[1],
                v0 + tri[2],
                vt0 + tri[2],
                vn0 + tri[2],
            );
        } else {
            let _ = writeln!(
                out,
                "f {}//{} {}//{} {}//{}",
                v0 + tri[0],
                vn0 + tri[0],
                v0 + tri[1],
                vn0 + tri[1],
                v0 + tri[2],
                vn0 + tri[2],
            );
        }
    }

    counters.vertices += positions.len() as u32;
    if with_uvs {
        counters.uvs += uv_data.len() as u32;
    }
    counters.normals += vertex_normals.len() as u32;
}

const MTL: &str = "\
# Blender: set Viewport Shading > Material Preview to see the leaf mask.
newmtl wood
Ka 0.20 0.18 0.15
Kd 0.24 0.19 0.14
Ks 0.00 0.00 0.00
d 1.0
illum 1

newmtl leaves
Ka 0.10 0.14 0.08
Kd 0.34 0.46 0.24
Ks 0.00 0.00 0.00
d 1.0
illum 1
map_Kd leaf_atlas.tga
map_d leaf_atlas.tga
";

/// Writes the alpha mask as an uncompressed 32-bit TGA (top-left origin).
///
/// The RGB channels duplicate the mask so it works both as `map_Kd` (white
/// inside the leaf, tinted by the material's green) and as `map_d` (white =
/// opaque leaf, black = transparent gap).
fn write_tga(path: &str, atlas: &AlphaMask) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(18 + atlas.alpha.len() * 4);
    bytes.extend_from_slice(&[0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    bytes.extend_from_slice(&(atlas.width as u16).to_le_bytes());
    bytes.extend_from_slice(&(atlas.height as u16).to_le_bytes());
    bytes.push(32);
    bytes.push(0x28); // 8 alpha bits, top-left origin
    for &a in &atlas.alpha {
        bytes.extend_from_slice(&[a, a, a, a]);
    }
    fs::write(path, bytes)
}
