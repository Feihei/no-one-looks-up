//! Diagnostic: procedural tree skeleton + baked-foliage stats (R3.9/R3.10).
//!
//! Run with `cargo run --example tree_stats`. Prints, per species, the
//! measured crown height/width, branch and leaf-cluster counts, and — for the
//! BAKED foliage — the card count, mean card size, the crown's sky-opening
//! rate (a side-view alpha-tested rasterisation, i.e. how much sky a crown
//! lets through) and the baked triangle count.
//!
//! Numbers, not vibes: the fragmentation direction (the locust's compound
//! leaves read as the finest silhouette) is judged by the opening rate and
//! card counts here.
//!
//! It also writes `target/tree_<species>.png`: the same side-view rasterisation
//! as the opening rate, but keeping wood and leaves as separate shades, so the
//! crown's macro shape ("one mass" vs "two brushes on sticks") can be looked at
//! without opening Blender.

use no_one_looks_up::world::leaf_shape::{ATLAS_HEIGHT, AlphaMask, bake_species_atlas};
use no_one_looks_up::world::tree_mesh::{
    TreeLod, TreeMeshKey, bake_tree_mesh_lod, leaf_card_stride,
};
use no_one_looks_up::world::tree_shape::{Branch, TREE_PROFILES, grow_tree};

/// Seed variants per species averaged.
const VARIANTS: u64 = 4;
/// Side-view rasterisation resolution (crown bbox mapped onto this width).
const RASTER: u32 = 480;

fn main() {
    println!(
        "{:<8} {:>2} {:>7} {:>7} {:>7} {:>6} {:>8}",
        "species", "id", "height", "width", "branches", "clusters", "proxy tris"
    );
    for (species, profile) in TREE_PROFILES.iter().enumerate() {
        let mut totals = (0usize, 0usize, 0usize);
        for variant in 0..VARIANTS {
            let tree = grow_tree(0x4845_4144, species as u8, variant);
            let tris = tree.branches.len() * 12 + tree.foliage.len() * 20;
            totals.0 += tree.branches.len();
            totals.1 += tree.foliage.len();
            totals.2 += tris;
            println!(
                "{:<8} {:>2} {:>6.1}m {:>6.1}m {:>7} {:>6} {:>8}",
                "variant",
                variant,
                tree.crown_height(),
                tree.crown_width(),
                tree.branches.len(),
                tree.foliage.len(),
                tris
            );
            if variant == 0 {
                for level in 0..=profile.max_level {
                    let of_level: Vec<&no_one_looks_up::world::tree_shape::Branch> =
                        tree.branches.iter().filter(|b| b.level == level).collect();
                    if of_level.is_empty() {
                        continue;
                    }
                    let total: f32 = of_level
                        .iter()
                        .map(|b| {
                            let d = [
                                b.end[0] - b.start[0],
                                b.end[1] - b.start[1],
                                b.end[2] - b.start[2],
                            ];
                            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
                        })
                        .sum();
                    let low = of_level.iter().map(|b| b.start[1]).fold(f32::MAX, f32::min);
                    println!(
                        "  L{:<6} {:>4} segs {:>6.2} m avg {:>5.1}..{:<5.1} m",
                        level,
                        of_level.len(),
                        total / of_level.len() as f32,
                        low,
                        of_level.iter().map(|b| b.end[1]).fold(f32::MIN, f32::max)
                    );
                }
            }
        }
        println!(
            "{:<8} {:>2} {:>6} {:>6} {:>7} {:>6} {:>8}   target {:.1}m x {:.1}m",
            "avg",
            species,
            "",
            "",
            totals.0 / VARIANTS as usize,
            totals.1 / VARIANTS as usize,
            totals.2 / VARIANTS as usize,
            profile.height,
            profile.width
        );
    }

    println!("\nbaked foliage (variant 0, Near LOD):");
    println!(
        "{:<8} {:>8} {:>10} {:>10} {:>10} {:>8}",
        "species", "cards", "card w (m)", "card h (m)", "opening", "tris"
    );
    let atlas = bake_species_atlas(ATLAS_HEIGHT);
    for species in 0..TREE_PROFILES.len() as u8 {
        let parts = bake_tree_mesh_lod(
            TreeMeshKey {
                species,
                variant: 0,
            },
            TreeLod::Near,
        );
        let positions = attribute_positions(&parts.leaves);
        let Some(uvs) = attribute_uvs(&parts.leaves) else {
            println!("species {species}: no leaf UVs");
            continue;
        };
        let indices = attribute_indices(&parts.leaves);
        let stride = leaf_card_stride(species);
        let cards = positions.len() / stride;
        let (mean_w, mean_h) = card_sizes(&positions, stride);
        let opening = sky_opening(&positions, &uvs, &indices, &atlas);
        let tris = parts.wood.count_vertices() / 3 + parts.leaves.count_vertices() / 3;
        println!(
            "{:<8} {:>8} {:>10.2} {:>10.2} {:>10.3} {:>8}",
            if species == 0 {
                "aspen"
            } else if species == 1 {
                "street"
            } else {
                "broadleaf"
            },
            cards,
            mean_w,
            mean_h,
            opening,
            tris
        );
    }
    println!(
        "\nopening = fraction of the crown's side-view bbox no leaf covers\n\
         proxy tris = wood vertices/3 + leaf vertices/3 (quads under-count)"
    );

    // Side-view previews, wood and leaves in separate shades: the only cheap
    // way to see whether the crown is one mass or a pair of brushes.
    let names = ["aspen", "street", "broadleaf"];
    for (species, name) in names.iter().enumerate() {
        let skeleton = grow_tree(0x4845_4144, species as u8, 0);
        let parts = bake_tree_mesh_lod(
            TreeMeshKey {
                species: species as u8,
                variant: 0,
            },
            TreeLod::Near,
        );
        let positions = attribute_positions(&parts.leaves);
        let uvs = attribute_uvs(&parts.leaves).unwrap_or_default();
        let indices = attribute_indices(&parts.leaves);
        let (w, h, rgb) = silhouette(&skeleton.branches, &positions, &uvs, &indices, &atlas, 0.0);
        let path = format!("target/tree_{name}.png");
        match write_png(&path, w, h, &rgb) {
            Ok(()) => println!("wrote {path}"),
            Err(err) => println!("{path}: {err}"),
        }
        // Same crown at the game's maximum upward tilt (60 degrees, ADR-0001):
        // an edge-on card projects to a line and contributes nothing, so this
        // image is the honest test of "what does the player actually see when
        // they look up".
        let (w, h, rgb) = silhouette(&skeleton.branches, &positions, &uvs, &indices, &atlas, 60.0);
        let path = format!("target/tree_{name}_up.png");
        match write_png(&path, w, h, &rgb) {
            Ok(()) => println!("wrote {path}"),
            Err(err) => println!("{path}: {err}"),
        }
    }
}

/// Side-view raster of the whole tree: wood in grey-brown, leaf cards in pale
/// green, sky in the background colour.
///
/// `elevation_deg` is the camera's tilt above the horizon: 0 is the ordinary
/// side view, 60 is the game's hard limit (ADR-0001), 90 would be straight up
/// the crown's axis. The screen's vertical axis is `y * cos - z * sin`, so a
/// card standing in a vertical plane loses area in exact proportion to how edge
/// on the player sees it — 90 is *not* the useful test, because the game never
/// looks up that far and a vertical willow shoot (柳条) then projects to almost
/// nothing while reading perfectly at 60.
fn silhouette(
    branches: &[Branch],
    positions: &[[f32; 3]],
    uvs: &[[f32; 2]],
    indices: &[u32],
    atlas: &AlphaMask,
    elevation_deg: f32,
) -> (u32, u32, Vec<u8>) {
    let (sin, cos) = elevation_deg.to_radians().sin_cos();
    // World (x, y, z) onto screen (x, up). The camera sits on -z looking up at
    // `elevation_deg`, so its up vector is (0, cos, -sin).
    let flatten = |p: [f32; 3]| -> (f32, f32) { (p[0], p[1] * cos - p[2] * sin) };
    let mut points: Vec<(f32, f32)> = positions.iter().map(|p| flatten(*p)).collect();
    for branch in branches {
        points.push(flatten(branch.start));
        points.push(flatten(branch.end));
    }
    let (mut x_min, mut x_max, mut y_min, mut y_max) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
    for p in &points {
        x_min = x_min.min(p.0);
        x_max = x_max.max(p.0);
        y_min = y_min.min(p.1);
        y_max = y_max.max(p.1);
    }
    let width = (x_max - x_min).max(0.5);
    let scale = (RASTER as f32 - 20.0) / width;
    let gw = RASTER;
    let gh = (((y_max - y_min) * scale).ceil() as u32 + 20).clamp(32, 4 * RASTER);
    let project = |p: [f32; 3]| {
        let (u, v) = flatten(p);
        ((u - x_min) * scale + 10.0, (y_max - v) * scale + 10.0)
    };
    let index = |x: u32, y: u32| (y * gw + x) as usize;
    let mut leaf = vec![false; (gw * gh) as usize];
    let mut wood = vec![false; (gw * gh) as usize];

    for branch in branches {
        let a = project(branch.start);
        let b = project(branch.end);
        let r = (branch.radius * scale * 0.5).max(0.7);
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) / 0.5)
            .ceil()
            .max(1.0) as u32;
        for step in 0..=steps {
            let t = step as f32 / steps as f32;
            let (px, py) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            let reach = r.ceil() as i32;
            for dy in -reach..=reach {
                for dx in -reach..=reach {
                    if ((dx * dx + dy * dy) as f32) > r * r + 0.25 {
                        continue;
                    }
                    let (x, y) = (px.round() as i32 + dx, py.round() as i32 + dy);
                    if x >= 0 && y >= 0 && (x as u32) < gw && (y as u32) < gh {
                        wood[index(x as u32, y as u32)] = true;
                    }
                }
            }
        }
    }

    // Index buffer, not quads: a shoot is a 3-segment strip.
    for tri in indices.chunks(3) {
        if tri.len() < 3 {
            break;
        }
        let corners: Vec<(f32, f32)> = tri
            .iter()
            .map(|i| project(positions[*i as usize]))
            .collect();
        let tri_uv = |k: usize| uvs[tri[k] as usize];
        let (a, b, c) = (corners[0], corners[1], corners[2]);
        let area = (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
        if area.abs() < 1e-6 {
            continue;
        }
        let min_x = a.0.min(b.0).min(c.0).floor().max(0.0) as u32;
        let max_x = a.0.max(b.0).max(c.0).ceil().min(gw as f32 - 1.0) as u32;
        let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as u32;
        let max_y = a.1.max(b.1).max(c.1).ceil().min(gh as f32 - 1.0) as u32;
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let p = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)) / area;
                let w1 = ((c.0 - b.0) * (p.1 - b.1) - (c.1 - b.1) * (p.0 - b.0)) / area;
                let w2 = ((a.0 - c.0) * (p.1 - c.1) - (a.1 - c.1) * (p.0 - c.0)) / area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let u = w0 * tri_uv(0)[0] + w1 * tri_uv(1)[0] + w2 * tri_uv(2)[0];
                let v = w0 * tri_uv(0)[1] + w1 * tri_uv(1)[1] + w2 * tri_uv(2)[1];
                let tx = (u * atlas.width as f32).clamp(0.0, atlas.width as f32 - 1.0) as u32;
                let ty = (v * atlas.height as f32).clamp(0.0, atlas.height as f32 - 1.0) as u32;
                if atlas.get(tx, ty) >= 128 {
                    leaf[index(px, py)] = true;
                }
            }
        }
    }

    let mut rgb = Vec::with_capacity((gw * gh * 3) as usize);
    for i in 0..(gw * gh) as usize {
        let pixel = if leaf[i] {
            [198u8, 206, 190]
        } else if wood[i] {
            [122u8, 110, 98]
        } else {
            [28u8, 30, 36]
        };
        rgb.extend_from_slice(&pixel);
    }
    (gw, gh, rgb)
}

/// Stored-deflate PNG writer (no image crate in this project).
fn write_png(
    path: &str,
    width: u32,
    height: u32,
    rgb: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut raw = Vec::with_capacity(rgb.len() + height as usize);
    for y in 0..height as usize {
        raw.push(0u8);
        raw.extend_from_slice(&rgb[y * width as usize * 3..(y + 1) * width as usize * 3]);
    }
    let compressed = zlib_stored(&raw);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &compressed);
    write_chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)?;
    Ok(())
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = table[((crc ^ *byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < data.len() {
        let len = (data.len() - offset).min(65535);
        let last = offset + len == data.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(&(!(len as u16)).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + len]);
        offset += len;
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn attribute_positions(mesh: &bevy::mesh::Mesh) -> Vec<[f32; 3]> {
    match mesh.attribute(bevy::mesh::Mesh::ATTRIBUTE_POSITION) {
        Some(bevy::mesh::VertexAttributeValues::Float32x3(v)) => v.clone(),
        _ => Vec::new(),
    }
}

fn attribute_uvs(mesh: &bevy::mesh::Mesh) -> Option<Vec<[f32; 2]>> {
    match mesh.attribute(bevy::mesh::Mesh::ATTRIBUTE_UV_0) {
        Some(bevy::mesh::VertexAttributeValues::Float32x2(v)) => Some(v.clone()),
        _ => None,
    }
}

fn attribute_indices(mesh: &bevy::mesh::Mesh) -> Vec<u32> {
    use bevy::mesh::Indices;
    match mesh.indices() {
        Some(Indices::U32(v)) => v.clone(),
        Some(Indices::U16(v)) => v.iter().map(|i| *i as u32).collect(),
        None => Vec::new(),
    }
}

/// Mean width/length of the leaf cards, in metres.
///
/// `stride` is one card's vertex count: 4 for a leaf quad, `2 * (STRAND_SEGMENTS
/// + 1)` for a shoot strip, whose length runs from the first row to the last.
fn card_sizes(positions: &[[f32; 3]], stride: usize) -> (f32, f32) {
    let mut sum = (0.0f32, 0.0f32);
    let mut n = 0usize;
    if stride < 4 {
        return (0.0, 0.0);
    }
    let mid = |a: [f32; 3], b: [f32; 3]| {
        [
            (a[0] + b[0]) * 0.5,
            (a[1] + b[1]) * 0.5,
            (a[2] + b[2]) * 0.5,
        ]
    };
    let d = |a: [f32; 3], b: [f32; 3]| {
        ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
    };
    for card in positions.chunks(stride) {
        if card.len() < stride {
            continue;
        }
        let start = mid(card[0], card[1]);
        let end = mid(card[stride - 2], card[stride - 1]);
        sum.0 += d(card[0], card[1]);
        sum.1 += d(start, end);
        n += 1;
    }
    if n == 0 {
        return (0.0, 0.0);
    }
    (sum.0 / n as f32, sum.1 / n as f32)
}

/// Rasterises the leaf quads from the side (+Z, slight yaw like the gallery)
/// and reports the fraction of the crown's projected bbox no leaf covers.
fn sky_opening(
    positions: &[[f32; 3]],
    uvs: &[[f32; 2]],
    indices: &[u32],
    atlas: &no_one_looks_up::world::leaf_shape::AlphaMask,
) -> f32 {
    if positions.is_empty() || uvs.len() != positions.len() {
        return 0.0;
    }
    // Project: x horizontal, y vertical (side view).
    let xs: Vec<f32> = positions.iter().map(|p| p[0]).collect();
    let ys: Vec<f32> = positions.iter().map(|p| p[1]).collect();
    let (x_min, x_max) = (
        xs.iter().cloned().fold(f32::MAX, f32::min),
        xs.iter().cloned().fold(f32::MIN, f32::max),
    );
    let (y_min, y_max) = (
        ys.iter().cloned().fold(f32::MAX, f32::min),
        ys.iter().cloned().fold(f32::MIN, f32::max),
    );
    let w = (x_max - x_min).max(1e-3);
    let h = (y_max - y_min).max(1e-3);
    let scale = RASTER as f32 / w;
    let gw = RASTER;
    let gh = ((h * scale).ceil() as u32).clamp(8, 4 * RASTER);
    let mut covered = vec![false; (gw * gh) as usize];

    let project = |p: [f32; 3]| ((p[0] - x_min) * scale, (y_max - p[1]) * scale);

    // Walk the index buffer instead of assuming quads: a shoot card is a
    // 3-segment strip, and only the mesh knows how many vertices that is.
    for tri in indices.chunks(3) {
        if tri.len() < 3 {
            break;
        }
        let corners: Vec<(f32, f32)> = tri
            .iter()
            .map(|i| project(positions[*i as usize]))
            .collect();
        let tri_uv = |k: usize| uvs[tri[k] as usize];
        let (a, b, c) = (corners[0], corners[1], corners[2]);
        let area = (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
        if area.abs() < 1e-6 {
            continue;
        }
        let min_x = a.0.min(b.0).min(c.0).floor().max(0.0) as u32;
        let max_x = a.0.max(b.0).max(c.0).ceil().min(gw as f32 - 1.0) as u32;
        let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as u32;
        let max_y = a.1.max(b.1).max(c.1).ceil().min(gh as f32 - 1.0) as u32;
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let p = (px as f32 + 0.5, py as f32 + 0.5);
                let w0 = ((b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)) / area;
                let w1 = ((c.0 - b.0) * (p.1 - b.1) - (c.1 - b.1) * (p.0 - b.0)) / area;
                let w2 = ((a.0 - c.0) * (p.1 - c.1) - (a.1 - c.1) * (p.0 - c.0)) / area;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let u = w0 * tri_uv(0)[0] + w1 * tri_uv(1)[0] + w2 * tri_uv(2)[0];
                let v = w0 * tri_uv(0)[1] + w1 * tri_uv(1)[1] + w2 * tri_uv(2)[1];
                let tx = (u * atlas.width as f32).clamp(0.0, atlas.width as f32 - 1.0) as u32;
                let ty = (v * atlas.height as f32).clamp(0.0, atlas.height as f32 - 1.0) as u32;
                if atlas.get(tx, ty) >= 128 {
                    covered[(py * gw + px) as usize] = true;
                }
            }
        }
    }

    let inside = covered.iter().filter(|c| **c).count();
    1.0 - inside as f32 / covered.len() as f32
}
