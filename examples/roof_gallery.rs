//! Roof silhouette contact sheet (R3.11 step 11.3).
//!
//! Run with `cargo run --example roof_gallery`. Writes, under the gitignored
//! draft area `.tmp/roof_gallery/`:
//!
//! - `roof_sheet.png` -- one panel per style, each showing the buildings the
//!   placement actually produces (old city: a row of 4-6 m shophouses; factory:
//!   one 48 x 50 m shed), drawn from a street-corner viewpoint with the roof
//!   geometry this step ships.
//!
//! Flat-shaded, painter's-algorithm, orthographic -- deliberately NOT pretty.
//! What it has to answer is binary and belongs to the skyline: does the gable
//! cut the sky, does the parapet read as a wall, do the sawtooth teeth zigzag.

use no_one_looks_up::world::facade::FacadeStyle;
use no_one_looks_up::world::roof::{RoofKind, RoofProfile, bake_gable, bake_tooth};

/// Camera: yaw around +Y and a shallow downward pitch, like standing on the
/// street and looking slightly up at the roofline.
const YAW: f32 = 0.45;
const PITCH: f32 = 0.18;

/// Sun direction for flat shading (not the game's 2.6 degree sun: this sheet
/// has to show the shapes, so the light is a readable studio light).
const SUN: [f32; 3] = [-0.45, 0.62, 0.64];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::Path::new(".tmp/roof_gallery");
    std::fs::create_dir_all(out_dir)?;

    let mut polys: Vec<Poly> = Vec::new();

    // Old city: a row of three shophouses along the street.
    let profile = RoofProfile::of(FacadeStyle::OldCity);
    for i in 0..3 {
        let x = i as f32 * 5.2;
        box_polygons(
            &mut polys,
            [x, 0.0, 0.0],
            [5.0, 4.5, 5.0],
            wall_tint(FacadeStyle::OldCity),
        );
        roof_polygons(
            &mut polys,
            profile,
            [x, 4.5, 0.0],
            [5.0, profile.rise_for(i as u8 % 2), 5.0],
            roof_tint(FacadeStyle::OldCity),
        );
    }

    // Apartment: one slab with its parapet ring.
    let profile = RoofProfile::of(FacadeStyle::Apartment);
    box_polygons(
        &mut polys,
        [22.0, 0.0, 0.0],
        [33.0, 18.0, 13.0],
        wall_tint(FacadeStyle::Apartment),
    );
    roof_polygons(
        &mut polys,
        profile,
        [22.0, 18.0, 0.0],
        [33.0, profile.rise_for(0), 13.0],
        roof_tint(FacadeStyle::Apartment),
    );

    // Factory: one shed with its teeth.
    let profile = RoofProfile::of(FacadeStyle::Factory);
    box_polygons(
        &mut polys,
        [60.0, 0.0, 0.0],
        [48.0, 24.0, 50.0],
        wall_tint(FacadeStyle::Factory),
    );
    roof_polygons(
        &mut polys,
        profile,
        [60.0, 24.0, 0.0],
        [48.0, profile.rise_for(1), 50.0],
        roof_tint(FacadeStyle::Factory),
    );

    let (width, height) = (1500usize, 560usize);
    let mut canvas = Canvas::new(width, height);
    canvas.ground_line();

    // Back-face cull, then painter's algorithm: farthest first. The first
    // version sorted descending, which painted the far wall over the near one
    // and made the sawtooth teeth float above an invisible factory.
    if std::env::var_os("ROOF_DEBUG").is_some() {
        for (i, p) in polys.iter().enumerate() {
            let xs: Vec<f32> = p.pts.iter().map(|p| p.0).collect();
            let ys: Vec<f32> = p.pts.iter().map(|p| p.1).collect();
            eprintln!(
                "poly {i}: n=({:.2},{:.2},{:.2}) depth {:.1} x {:.0}-{:.0} y {:.0}-{:.0}",
                p.normal[0],
                p.normal[1],
                p.normal[2],
                p.depth,
                xs.iter().cloned().fold(f32::INFINITY, f32::min),
                xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
                ys.iter().cloned().fold(f32::INFINITY, f32::min),
                ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            );
        }
    }
    polys.retain(|p| p.normal[2] > 0.0);
    polys.sort_by(|a, b| {
        a.depth
            .partial_cmp(&b.depth)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for poly in &polys {
        canvas.fill_poly(&poly.pts, shade(poly.tint, poly.normal));
    }

    write_png(
        &out_dir.join("roof_sheet.png"),
        width,
        height,
        &canvas.pixels,
    )?;
    println!("saved {}", out_dir.join("roof_sheet.png").display());
    Ok(())
}

// --- geometry --------------------------------------------------------------

/// One drawable polygon in camera space.
struct Poly {
    pts: Vec<(f32, f32)>,
    /// Camera-space normal; its z component culls back faces.
    normal: [f32; 3],
    tint: [f32; 3],
    /// Mean camera depth. LARGER is closer to the camera.
    depth: f32,
}

fn wall_tint(style: FacadeStyle) -> [f32; 3] {
    match style {
        FacadeStyle::OldCity => [0.54, 0.50, 0.44],
        FacadeStyle::Apartment => [0.47, 0.47, 0.49],
        FacadeStyle::Factory => [0.27, 0.26, 0.25],
        FacadeStyle::UrbanVillage => [0.55, 0.50, 0.42],
    }
}

fn roof_tint(style: FacadeStyle) -> [f32; 3] {
    RoofProfile::of(style).tint
}

/// Adds the wall box of one building (six faces, back faces culled later).
fn box_polygons(polys: &mut Vec<Poly>, base: [f32; 3], size: [f32; 3], tint: [f32; 3]) {
    let [x, y, z] = base;
    let [l, h, d] = size;
    let (x0, x1) = (x - l * 0.5, x + l * 0.5);
    let (z0, z1) = (z - d * 0.5, z + d * 0.5);
    let (y0, y1) = (y, y + h);
    push_quad(
        polys,
        [x0, y0, z1],
        [x1, y0, z1],
        [x1, y1, z1],
        [x0, y1, z1],
        tint,
    );
    push_quad(
        polys,
        [x1, y0, z0],
        [x0, y0, z0],
        [x0, y1, z0],
        [x1, y1, z0],
        tint,
    );
    push_quad(
        polys,
        [x0, y0, z0],
        [x0, y0, z1],
        [x0, y1, z1],
        [x0, y1, z0],
        tint,
    );
    push_quad(
        polys,
        [x1, y0, z1],
        [x1, y0, z0],
        [x1, y1, z0],
        [x1, y1, z1],
        tint,
    );
    push_quad(
        polys,
        [x0, y1, z1],
        [x1, y1, z1],
        [x1, y1, z0],
        [x0, y1, z0],
        tint,
    );
}

/// Adds one style's roof above the wall top.
fn roof_polygons(
    polys: &mut Vec<Poly>,
    profile: RoofProfile,
    base: [f32; 3],
    size: [f32; 3],
    tint: [f32; 3],
) {
    let [x, y, z] = base;
    let [l, rise, d] = size;
    match profile.kind {
        RoofKind::Gable => {
            // The baked mesh is unit; scale it here. Triangles straight from the
            // baker, so the sheet shows exactly what ships.
            for tri in mesh_triangles(&bake_gable()) {
                push_tri_world(polys, tri, [x, y, z], [l, rise, d], tint);
            }
        }
        RoofKind::Parapet => {
            const WALL: f32 = 0.25;
            let walls = [
                ([0.0, 0.0, (d - WALL) * 0.5], [l, rise, WALL]),
                ([0.0, 0.0, -(d - WALL) * 0.5], [l, rise, WALL]),
                ([(l - WALL) * 0.5, 0.0, 0.0], [WALL, rise, d - WALL * 2.0]),
                ([-(l - WALL) * 0.5, 0.0, 0.0], [WALL, rise, d - WALL * 2.0]),
            ];
            for (offset, size) in walls {
                box_polygons(
                    polys,
                    [x + offset[0], y + offset[1], z + offset[2]],
                    size,
                    tint,
                );
            }
        }
        RoofKind::Sawtooth => {
            let (count, width) = profile.teeth(l);
            for tri in mesh_triangles(&bake_tooth()) {
                for i in 0..count {
                    let lx = -l * 0.5 + (i as f32 + 0.5) * width;
                    push_tri_world(polys, tri, [x + lx, y, z], [width, rise, d], tint);
                }
            }
        }
    }
}

/// Triangles of a baked mesh as `(a, b, c)` triples.
fn mesh_triangles(mesh: &bevy::mesh::Mesh) -> Vec<[[f32; 3]; 3]> {
    let positions: Vec<[f32; 3]> = mesh
        .attribute(bevy::mesh::Mesh::ATTRIBUTE_POSITION)
        .expect("positions")
        .as_float3()
        .expect("f32")
        .to_vec();
    let indices: Vec<u32> = match mesh.indices().expect("indices") {
        bevy::mesh::Indices::U32(i) => i.to_vec(),
        bevy::mesh::Indices::U16(i) => i.iter().map(|&v| v as u32).collect(),
    };
    #[allow(clippy::chunks_exact_to_as_chunks)] // chunk size is a fixed triangle
    indices
        .chunks_exact(3)
        .map(|t| {
            [
                positions[t[0] as usize],
                positions[t[1] as usize],
                positions[t[2] as usize],
            ]
        })
        .collect()
}

fn push_tri_world(
    polys: &mut Vec<Poly>,
    tri: [[f32; 3]; 3],
    origin: [f32; 3],
    scale: [f32; 3],
    tint: [f32; 3],
) {
    let world: Vec<[f32; 3]> = tri
        .iter()
        .map(|p| {
            [
                origin[0] + p[0] * scale[0],
                origin[1] + p[1] * scale[1],
                origin[2] + p[2] * scale[2],
            ]
        })
        .collect();
    push_tri(polys, world[0], world[1], world[2], tint);
}

fn push_tri(polys: &mut Vec<Poly>, a: [f32; 3], b: [f32; 3], c: [f32; 3], tint: [f32; 3]) {
    let n = face_normal(a, b, c);
    let (depth, pts, cam_n) = project(&[a, b, c], n);
    // No near-plane test here: `depth` is a signed orthographic coordinate, not
    // a distance, and half the scene sits at negative z. Degenerate polygons
    // simply rasterise to nothing.
    polys.push(Poly {
        pts,
        normal: cam_n,
        tint,
        depth,
    });
}

fn push_quad(
    polys: &mut Vec<Poly>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    tint: [f32; 3],
) {
    let n = face_normal(a, b, c);
    let (depth, pts, cam_n) = project(&[a, b, c, d], n);
    polys.push(Poly {
        pts,
        normal: cam_n,
        tint,
        depth,
    });
}

fn face_normal(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    [
        ab[1] * ac[2] - ab[2] * ac[1],
        ab[2] * ac[0] - ab[0] * ac[2],
        ab[0] * ac[1] - ab[1] * ac[0],
    ]
}

/// Orthographic projection with the gallery's fixed yaw/pitch. Returns the
/// pixel polygon and the mean camera-space depth (larger = closer here, since
/// the camera sits at +z looking back).
fn project(points: &[[f32; 3]], normal: [f32; 3]) -> (f32, Vec<(f32, f32)>, [f32; 3]) {
    let (sin_y, cos_y) = YAW.sin_cos();
    let (sin_p, cos_p) = PITCH.sin_cos();
    let mut pts = Vec::with_capacity(points.len());
    let mut depth = 0.0;
    for p in points {
        // Yaw about +Y, then pitch about +X.
        let x = p[0] * cos_y + p[2] * sin_y;
        let z = -p[0] * sin_y + p[2] * cos_y;
        let y = p[1] * cos_p - z * sin_p;
        let zc = p[1] * sin_p + z * cos_p;
        pts.push((240.0 + x * 6.0, 430.0 - y * 6.0));
        depth += zc;
    }
    // Rotate the normal the same way, so its camera-space z says which way the
    // face points relative to the viewer.
    let nx = normal[0] * cos_y + normal[2] * sin_y;
    let nz = -normal[0] * sin_y + normal[2] * cos_y;
    let ny = normal[1] * cos_p - nz * sin_p;
    let nzc = normal[1] * sin_p + nz * cos_p;
    (depth / points.len() as f32, pts, [nx, ny, nzc])
}

fn shade(tint: [f32; 3], normal: [f32; 3]) -> [u8; 3] {
    let len = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    let n = [normal[0] / len, normal[1] / len, normal[2] / len];
    // Sun + a floor of ambient, so back faces stay readable.
    let light = (SUN[0] * n[0] + SUN[1] * n[1] + SUN[2] * n[2]).clamp(0.0, 1.0);
    let k = 0.35 + 0.65 * light;
    [
        (tint[0] * k).clamp(0.0, 1.0),
        (tint[1] * k).clamp(0.0, 1.0),
        (tint[2] * k).clamp(0.0, 1.0),
    ]
    .map(|c| (c * 255.0) as u8)
}

// --- canvas ----------------------------------------------------------------

struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: [24, 26, 30, 255].repeat(width * height),
        }
    }

    fn ground_line(&mut self) {
        let y = 430;
        for x in 0..self.width {
            self.put(x, y, [54, 56, 60]);
        }
    }

    fn put(&mut self, x: usize, y: usize, color: [u8; 3]) {
        if x < self.width && y < self.height {
            let i = (y * self.width + x) * 4;
            self.pixels[i..i + 3].copy_from_slice(&color);
        }
    }

    /// Scanline fill of a convex polygon.
    fn fill_poly(&mut self, pts: &[(f32, f32)], color: [u8; 3]) {
        let min_y = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
        let max_y = pts.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max);
        for y in (min_y.floor() as usize)..=(max_y.ceil() as usize) {
            let yf = y as f32 + 0.5;
            let mut xs: Vec<f32> = Vec::new();
            for i in 0..pts.len() {
                let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                if (a.1 <= yf && b.1 > yf) || (b.1 <= yf && a.1 > yf) {
                    let t = (yf - a.1) / (b.1 - a.1);
                    xs.push(a.0 + (b.0 - a.0) * t);
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks(2) {
                if pair.len() == 2 {
                    for x in (pair[0].ceil() as usize)..=(pair[1].floor() as usize) {
                        self.put(x, y, color);
                    }
                }
            }
        }
    }
}

// --- PNG (stored deflate, same as the other galleries) ---------------------

fn write_png(
    path: &std::path::Path,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(rgba.len(), width * height * 4, "rgba size mismatch");
    let mut raw = Vec::with_capacity(rgba.len() + height);
    for row in rgba.chunks(width * 4) {
        raw.push(0u8);
        raw.extend_from_slice(row);
    }
    let compressed = zlib_stored(&raw);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
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
