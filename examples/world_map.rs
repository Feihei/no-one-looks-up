//! Dev tool: rasterize the generated world layout to a top-down PNG.
//!
//! Runs the real generation (`generate_world`) and draws the result with a
//! tiny software rasterizer — no GPU, no window, deterministic output. This
//! is the reliable way to review the *algorithm* output; the in-game Gizmos
//! overlay is for walking around in 3D.
//!
//! Usage:
//!
//! ```text
//! cargo run --example world_map
//! # optional: SEED=0x1234 cargo run --example world_map
//! #           WORLD_MAP_OUT=world_map.png cargo run --example world_map
//! ```
//!
//! Output: `target/world_map.png` (default), 2 px/m so the canvas tracks
//! `WORLD_SIZE` (800 m -> 1600x1600 px), north
//! (-Z / row 0) at the top, west (x = 0) at the left.
//!
//! Colors:
//! - background: walkable street (dusty asphalt)
//! - block fill by type: old city red / apartment blue / factory yellow /
//!   plaza green / forest dark green
//! - block outline: near-black (the inset polygon edge = visible street curb)
//! - junction nodes: white dots

use std::collections::HashMap;

use bevy::math::Vec2;
use no_one_looks_up::world::layout_graph::{BlockType, NodeKind};
use no_one_looks_up::world::path_network::generate_world;
use no_one_looks_up::world::{CELL_SIZE, GRID_CELLS, WORLD_SIZE};

/// Pixels per metre. 2 px/m keeps 6-10 m streets legible; derived from
/// `WORLD_SIZE` so resizing the map never leaves the canvas mismatched.
const PX_PER_M: f32 = 2.0;
/// Canvas edge in pixels (`2 px/m * WORLD_SIZE`).
const PX: usize = (PX_PER_M as usize) * (WORLD_SIZE as usize);

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const STREET: Rgb = Rgb(54, 54, 58);
const OUTLINE: Rgb = Rgb(20, 20, 22);
const NODE: Rgb = Rgb(255, 255, 255);

fn block_color(kind: BlockType) -> Rgb {
    match kind {
        BlockType::OldCity => Rgb(186, 78, 70),
        BlockType::UrbanVillage => Rgb(206, 128, 74),
        BlockType::Apartment => Rgb(88, 120, 180),
        BlockType::Factory => Rgb(196, 172, 78),
        BlockType::Plaza => Rgb(96, 176, 96),
        BlockType::Forest => Rgb(46, 110, 74),
    }
}

struct Canvas {
    px: Vec<Rgb>,
}

impl Canvas {
    fn new() -> Self {
        Self {
            px: vec![STREET; PX * PX],
        }
    }

    fn set(&mut self, x: i64, y: i64, color: Rgb) {
        if x >= 0 && y >= 0 && (x as usize) < PX && (y as usize) < PX {
            self.px[y as usize * PX + x as usize] = color;
        }
    }

    /// Fills a convex polygon (even-odd scanline, 1x1 pixel centers).
    fn fill_polygon(&mut self, poly: &[Vec2], color: Rgb) {
        if poly.len() < 3 {
            return;
        }
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in poly {
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
        }
        let y0 = ((min_y * PX_PER_M).floor() as i64).max(0);
        let y1 = ((max_y * PX_PER_M).ceil() as i64).min(PX as i64 - 1);
        for y in y0..=y1 {
            let sy = (y as f32 + 0.5) * 0.5;
            let mut xs: Vec<f32> = Vec::with_capacity(4);
            for i in 0..poly.len() {
                let a = poly[i];
                let b = poly[(i + 1) % poly.len()];
                if (a.y <= sy && b.y > sy) || (b.y <= sy && a.y > sy) {
                    let t = (sy - a.y) / (b.y - a.y);
                    xs.push(a.x + t * (b.x - a.x));
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks(2) {
                if pair.len() < 2 {
                    continue;
                }
                let xa = (pair[0] * PX_PER_M).floor() as i64;
                let xb = (pair[1] * PX_PER_M).ceil() as i64;
                for x in xa..xb {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// Draws a 1 px line (Bresenham) between two world points.
    fn line(&mut self, a: Vec2, b: Vec2, color: Rgb) {
        let (mut x0, mut y0) = (
            (a.x * PX_PER_M).round() as i64,
            (a.y * PX_PER_M).round() as i64,
        );
        let (x1, y1) = (
            (b.x * PX_PER_M).round() as i64,
            (b.y * PX_PER_M).round() as i64,
        );
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.set(x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    fn polygon_outline(&mut self, poly: &[Vec2], color: Rgb) {
        for i in 0..poly.len() {
            self.line(poly[i], poly[(i + 1) % poly.len()], color);
        }
    }

    fn disc(&mut self, center: Vec2, radius: f32, color: Rgb) {
        let r = radius.ceil() as i64;
        for dy in -r..=r {
            for dx in -r..=r {
                if ((dx * dx + dy * dy) as f32).sqrt() <= radius {
                    self.set(
                        (center.x * PX_PER_M) as i64 + dx,
                        (center.y * PX_PER_M) as i64 + dy,
                        color,
                    );
                }
            }
        }
    }
}

/// Writes an 8-bit RGB PNG (file size is the only reason to prefer this over
/// a Bevy image asset: zero GPU involvement).
fn write_png(path: &str, width: usize, height: usize, px: &[Rgb]) -> std::io::Result<()> {
    use std::io::Write;

    let mut raw = Vec::with_capacity((width * 3 + 1) * height);
    for y in 0..height {
        raw.push(0); // filter: none
        for x in 0..width {
            let Rgb(r, g, b) = px[y * width + x];
            raw.extend_from_slice(&[r, g, b]);
        }
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

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let mut crc_body = Vec::with_capacity(body.len() + 4);
        crc_body.extend_from_slice(kind);
        crc_body.extend_from_slice(body);
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&crc32(&crc_body).to_be_bytes());
    }

    // zlib stream of stored (uncompressed) deflate blocks.
    let mut zlib = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < raw.len() {
        let take = (raw.len() - offset).min(65535);
        let last = offset + take == raw.len();
        zlib.push(if last { 1 } else { 0 });
        zlib.extend_from_slice(&(take as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(take as u16)).to_le_bytes());
        zlib.extend_from_slice(&raw[offset..offset + take]);
        offset += take;
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);

    std::fs::File::create(path)?.write_all(&png)
}

fn main() {
    let seed = std::env::var("SEED")
        .ok()
        .map(|raw| {
            let raw = raw.trim().to_string();
            match raw.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).unwrap_or_default(),
                None => raw.parse().unwrap_or_default(),
            }
        })
        .unwrap_or(no_one_looks_up::world::chunks::DEFAULT_SEED);
    let out = std::env::var("WORLD_MAP_OUT").unwrap_or_else(|_| "target/world_map.png".to_string());

    let (grid, graph) = match generate_world(seed) {
        Ok(world) => world,
        Err(err) => {
            eprintln!("generation failed: {err}");
            std::process::exit(1);
        }
    };
    println!(
        "seed {seed:#x}: {} blocks, {} street segments, {} nodes ({} junctions, {} corners, {} plazas)",
        graph.blocks.len(),
        graph.edges.len(),
        graph.nodes.len(),
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Junction)
            .count(),
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Corner)
            .count(),
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Plaza)
            .count()
    );

    let mut canvas = Canvas::new();

    // Blocks: fill by type, then outline the inset polygon (the street curb).
    for block in &graph.blocks {
        canvas.fill_polygon(&block.polygon, block_color(block.kind));
        canvas.polygon_outline(&block.polygon, OUTLINE);
    }

    // Street centerlines from the layout graph (helps read the network up
    // close; subtle so the block fill still dominates).
    for edge in &graph.edges {
        if let (Some(a), Some(b)) = (
            graph.nodes.get(edge.from as usize),
            graph.nodes.get(edge.to as usize),
        ) {
            canvas.line(a.pos, b.pos, Rgb(80, 80, 86));
        }
    }

    // Junction nodes.
    for node in &graph.nodes {
        match node.kind {
            NodeKind::Junction => canvas.disc(node.pos, 3.0, NODE),
            NodeKind::Corner => canvas.disc(node.pos, 1.0, Rgb(150, 150, 160)),
            NodeKind::Plaza => {}
        }
    }

    // Frame + place reference marks every 100 m (faint).
    let world = WORLD_SIZE;
    canvas.line(Vec2::ZERO, Vec2::new(world, 0.0), OUTLINE);
    canvas.line(Vec2::new(0.0, world), Vec2::new(world, world), OUTLINE);
    canvas.line(Vec2::ZERO, Vec2::new(0.0, world), OUTLINE);
    canvas.line(Vec2::new(world, 0.0), Vec2::new(world, world), OUTLINE);
    for i in 1..8 {
        let v = i as f32 * 100.0;
        canvas.disc(Vec2::new(v, 0.0 + 4.0), 1.0, NODE);
        canvas.disc(Vec2::new(4.0, v), 1.0, NODE);
    }

    match write_png(&out, PX, PX, &canvas.px) {
        Ok(()) => println!("world map saved to {out} ({PX}x{PX}, {PX_PER_M} px = 1 m)"),
        Err(err) => {
            eprintln!("failed to write {out}: {err}");
            std::process::exit(1);
        }
    }

    // Stats for review: block type mix and street share of the map.
    let mut mix: HashMap<&str, usize> = HashMap::new();
    for block in &graph.blocks {
        let key = match block.kind {
            BlockType::OldCity => "old_city",
            BlockType::UrbanVillage => "urban_village",
            BlockType::Apartment => "apartment",
            BlockType::Factory => "factory",
            BlockType::Plaza => "plaza",
            BlockType::Forest => "forest",
        };
        *mix.entry(key).or_default() += 1;
    }
    let roads = grid.cells.iter().filter(|c| c.is_road).count();
    println!(
        "cells: {} total, {roads} street-adjacent ({:.0}%); block mix: {:?}",
        grid.cells.len(),
        roads as f32 / grid.cells.len() as f32 * 100.0,
        mix
    );
    let _ = (CELL_SIZE, GRID_CELLS);
}
