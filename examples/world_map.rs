//! Dev tool: rasterize the generated world to top-down PNGs.
//!
//! Runs the real generation (`generate_world`) plus the real placement
//! (`plan_chunk`) and draws the result with a tiny software rasterizer — no
//! GPU, no window, deterministic output. This is the reliable way to review
//! the *algorithm* output; the in-game Gizmos overlay is for walking around
//! in 3D.
//!
//! Usage:
//!
//! ```text
//! cargo run --example world_map
//! # optional: SEED=0x1234 cargo run --example world_map
//! #           WORLD_MAP_OUT=world_map.png cargo run --example world_map
//! #           WORLD_MAP_PX=4 cargo run --example world_map   (px per metre)
//! #           WORLD_MAP_CROPS=0 cargo run --example world_map (skip close-ups)
//! ```
//!
//! Output:
//! - `<out>`: the whole 800 m map at `WORLD_MAP_PX` px/m (default 3).
//! - `<out stem>_village.png` / `_oldcity.png` / `_apartment.png`: 1:1
//!   close-ups (12 px/m, 100 m wide) centred on the densest district of each
//!   subdivided type, so the tier-1 lanes (ADR-0008) are actually legible.
//!
//! Drawing order (matters): street ground -> block face -> sub-block plots ->
//! building footprints -> lane centerlines -> curbs -> nodes.
//!
//! Colors:
//! - background: walkable street (dusty asphalt); lanes read as the same
//!   ground, one shade lighter
//! - plot fill by block type: old city red / urban village orange /
//!   apartment blue / factory yellow / plaza green / forest dark green
//! - building footprint: the type colour, darkened and outlined
//! - lane centerline: pale cyan (tier 1), street centerline: warm grey (tier 0)
//! - nodes: white junctions, grey corners, cyan lane mouths

use std::collections::HashMap;

use bevy::math::Vec2;
use no_one_looks_up::world::layout_graph::{BlockType, EdgeKind, LayoutGraph, NodeKind};
use no_one_looks_up::world::path_network::generate_world;
use no_one_looks_up::world::placement::{PlacedKind, plan_chunk, rotated_corners};
use no_one_looks_up::world::{CELL_SIZE, CHUNK_SIZE, GRID_CELLS, WORLD_SIZE};

/// Default pixels per metre for the whole-map bake.
const DEFAULT_PX_PER_M: f32 = 3.0;
/// Pixels per metre for the close-ups (1 m = 12 px: a 2.4 m lane is 29 px).
const CROP_PX_PER_M: f32 = 12.0;
/// Width of a close-up window, in metres.
const CROP_WORLD_WIDTH: f32 = 100.0;

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

const STREET: Rgb = Rgb(52, 52, 57);
const LANE_GROUND: Rgb = Rgb(62, 63, 68);
const OUTLINE: Rgb = Rgb(18, 18, 20);
const NODE: Rgb = Rgb(255, 255, 255);
const LANE_LINE: Rgb = Rgb(96, 150, 175);
const STREET_LINE: Rgb = Rgb(86, 84, 80);
/// ADR-0009: the arterial cross, brighter than a street centerline.
const ARTERIAL_LINE: Rgb = Rgb(128, 124, 118);

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

/// Multiplies a colour's channels (keeps the type hue, changes the value).
fn shade(color: Rgb, factor: f32) -> Rgb {
    let f = |c: u8| ((c as f32 * factor).round().clamp(0.0, 255.0)) as u8;
    Rgb(f(color.0), f(color.1), f(color.2))
}

/// A square canvas covering a world-space window.
struct Canvas {
    px: Vec<Rgb>,
    size: usize,
    px_per_m: f32,
    /// World position of the window's top-left corner (min x, min z).
    origin: Vec2,
}

impl Canvas {
    fn new(origin: Vec2, world_size: f32, px_per_m: f32) -> Self {
        let size = (world_size * px_per_m).round().max(1.0) as usize;
        Self {
            px: vec![STREET; size * size],
            size,
            px_per_m,
            origin,
        }
    }

    /// World XZ -> pixel coordinates of the pixel containing that point.
    fn to_px(&self, p: Vec2) -> (i64, i64) {
        (
            ((p.x - self.origin.x) * self.px_per_m).floor() as i64,
            ((p.y - self.origin.y) * self.px_per_m).floor() as i64,
        )
    }

    fn set(&mut self, x: i64, y: i64, color: Rgb) {
        if x >= 0 && y >= 0 && (x as usize) < self.size && (y as usize) < self.size {
            self.px[y as usize * self.size + x as usize] = color;
        }
    }

    /// Fills a polygon (even-odd scanline, pixel centres).
    fn fill_polygon(&mut self, poly: &[Vec2], color: Rgb) {
        if poly.len() < 3 {
            return;
        }
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in poly {
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
        }
        let inv = 1.0 / self.px_per_m;
        let y0 = (((min_y - self.origin.y) * self.px_per_m).floor() as i64).max(0);
        let y1 =
            (((max_y - self.origin.y) * self.px_per_m).ceil() as i64).min(self.size as i64 - 1);
        for y in y0..=y1 {
            // Pixel centre back in world space.
            let sy = self.origin.y + (y as f32 + 0.5) * inv;
            let mut xs: Vec<f32> = Vec::with_capacity(8);
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
                let xa = (((pair[0] - self.origin.x) * self.px_per_m).floor() as i64).max(0);
                let xb = (((pair[1] - self.origin.x) * self.px_per_m).ceil() as i64)
                    .min(self.size as i64);
                for x in xa..xb {
                    self.set(x, y, color);
                }
            }
        }
    }

    /// Draws a line of `width_px` pixels (thick lines are square dabs).
    fn line(&mut self, a: Vec2, b: Vec2, color: Rgb, width_px: i32) {
        let (mut x0, mut y0) = self.to_px(a);
        let (x1, y1) = self.to_px(b);
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let half = width_px.max(1) as i64 / 2;
        loop {
            for oy in -half..=half {
                for ox in -half..=half {
                    self.set(x0 + ox, y0 + oy, color);
                }
            }
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

    fn polygon_outline(&mut self, poly: &[Vec2], color: Rgb, width_px: i32) {
        for i in 0..poly.len() {
            self.line(poly[i], poly[(i + 1) % poly.len()], color, width_px);
        }
    }

    fn disc(&mut self, center: Vec2, radius_px: f32, color: Rgb) {
        let (cx, cy) = self.to_px(center);
        let r = radius_px.ceil() as i64;
        for dy in -r..=r {
            for dx in -r..=r {
                if ((dx * dx + dy * dy) as f32).sqrt() <= radius_px {
                    self.set(cx + dx, cy + dy, color);
                }
            }
        }
    }

    fn save(&self, path: &str) -> std::io::Result<()> {
        write_png(path, self.size, self.size, &self.px)
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

/// Every building planned for the world, as `(corners, kind)`.
fn planned_buildings(graph: &LayoutGraph, seed: u64) -> Vec<([Vec2; 4], BlockType)> {
    let cells = (CELL_SIZE * GRID_CELLS as f32 / CHUNK_SIZE) as i32;
    let mut out = Vec::new();
    for cx in 0..cells {
        for cz in 0..cells {
            for object in plan_chunk(graph, seed, (cx, cz), 1).objects {
                let PlacedKind::Building(kind) = object.kind else {
                    continue;
                };
                let corners =
                    rotated_corners(object.position, object.yaw, object.size.x, object.size.z);
                out.push((corners, kind));
            }
        }
    }
    out
}

/// Draws the layout into `canvas`: plots, buildings, lanes, curbs, nodes.
fn draw_world(
    canvas: &mut Canvas,
    graph: &LayoutGraph,
    buildings: &[([Vec2; 4], BlockType)],
    px: f32,
) {
    let curb_w = (0.35 * px).round().max(1.0) as i32;
    let plot_outline_w = (0.12 * px).round().max(1.0) as i32;
    let lane_line_w = (0.18 * px).round().max(1.0) as i32;

    // 1. Lane corridors: paint the block's territory as lane ground first, so
    //    every gap the plots leave reads as walkable alley.
    for block in &graph.blocks {
        if block.children.is_empty() || block.polygon.len() < 3 {
            continue;
        }
        canvas.fill_polygon(&block.polygon, LANE_GROUND);
    }

    // 2. Sub-block plots (or the whole block when it is not subdivided),
    //    plus their outlines. A slight per-plot shade keeps neighbouring
    //    plots legible without hiding the type colour.
    for block in &graph.blocks {
        let base = block_color(block.kind);
        if block.children.is_empty() {
            if block.polygon.len() >= 3 {
                canvas.fill_polygon(&block.polygon, base);
            }
            continue;
        }
        for (i, child) in block.children.iter().enumerate() {
            let factor = [1.0, 0.93, 1.05, 0.97][i % 4];
            canvas.fill_polygon(&child.polygon, shade(base, factor));
            canvas.polygon_outline(&child.polygon, shade(base, 0.55), plot_outline_w);
        }
    }

    // 3. Centerlines: tier-0 warm grey, tier-1 cyan (the ADR's hierarchy).
    //    Drawn BEFORE the buildings: a lane line is the seam between two
    //    rows, so drawing it afterwards painted blue lines across the roofs
    //    and made the lanes look like they ran through the houses.
    for edge in &graph.edges {
        let (Some(a), Some(b)) = (
            graph.nodes.get(edge.from as usize),
            graph.nodes.get(edge.to as usize),
        ) else {
            continue;
        };
        let (color, width) = match edge.kind {
            // ADR-0009: the arterial cross draws wider and brighter than a
            // street, under the same warm-grey family.
            EdgeKind::Street => (STREET_LINE, curb_w.max(1)),
            EdgeKind::Arterial => (ARTERIAL_LINE, (curb_w as f32 * 1.8).max(2.0) as i32),
            EdgeKind::Lane => (LANE_LINE, lane_line_w),
        };
        canvas.line(a.pos, b.pos, color, width);
    }

    // 4. Building footprints: much darker than the plot fill, so the rows
    //    read as built volume at a glance. The old 0.72 factor left an
    //    apartment block (blue plot / blue-grey slab) looking empty even at
    //    60% coverage.
    for (corners, kind) in buildings {
        canvas.fill_polygon(corners, shade(block_color(*kind), 0.45));
    }

    // 5. Curbs: the block's own inset polygon, near-black.
    for block in &graph.blocks {
        if block.polygon.len() >= 3 {
            canvas.polygon_outline(&block.polygon, OUTLINE, curb_w);
        }
    }

    // 6. Nodes: junctions, corners, lane mouths.
    let junction_r = (1.1 * px).max(2.0);
    let mouth_r = (0.7 * px).max(1.5);
    for node in &graph.nodes {
        match node.kind {
            NodeKind::Junction => canvas.disc(node.pos, junction_r, NODE),
            NodeKind::Corner => canvas.disc(node.pos, junction_r * 0.45, Rgb(150, 150, 160)),
            NodeKind::LaneMouth => canvas.disc(node.pos, mouth_r, Rgb(90, 190, 230)),
            // Straight mid-run anchors: no marker (R3.20).
            NodeKind::Pass | NodeKind::Plaza => {}
        }
    }
}

/// Centre of the tightest cluster of blocks of `kind` (the one with the most
/// same-kind neighbours within 120 m), for the close-up windows.
fn densest_center(graph: &LayoutGraph, kind: BlockType) -> Option<Vec2> {
    // Only the subdivided kinds need plots; R3.20 apartment districts keep
    // their interior whole and must still get a close-up.
    let subdivided = matches!(kind, BlockType::UrbanVillage | BlockType::OldCity);
    let centers: Vec<Vec2> = graph
        .blocks
        .iter()
        .filter(|b| b.kind == kind && (!subdivided || !b.children.is_empty()))
        .map(|b| b.center())
        .collect();
    let mut best: Option<(usize, Vec2)> = None;
    for a in &centers {
        let count = centers.iter().filter(|b| a.distance(**b) <= 120.0).count();
        if best.is_none_or(|(n, _)| count > n) {
            best = Some((count, *a));
        }
    }
    best.map(|(_, c)| c)
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
    let px_per_m = std::env::var("WORLD_MAP_PX")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|v| (1.0..=12.0).contains(v))
        .unwrap_or(DEFAULT_PX_PER_M);
    let make_crops = std::env::var("WORLD_MAP_CROPS").map(|v| v.trim() != "0") != Ok(false);

    let (grid, graph) = match generate_world(seed) {
        Ok(world) => world,
        Err(err) => {
            eprintln!("generation failed: {err}");
            std::process::exit(1);
        }
    };
    let lanes = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Lane)
        .count();
    let streets = graph.edges.len() - lanes;
    println!(
        "seed {seed:#x}: {} blocks ({} subdivided, {} plots), {streets} street segments + {lanes} lanes, {} nodes ({} junctions, {} corners, {} lane mouths, {} plazas)",
        graph.blocks.len(),
        graph
            .blocks
            .iter()
            .filter(|b| !b.children.is_empty())
            .count(),
        graph.blocks.iter().map(|b| b.children.len()).sum::<usize>(),
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
            .filter(|n| n.kind == NodeKind::LaneMouth)
            .count(),
        graph
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Plaza)
            .count()
    );

    let buildings = planned_buildings(&graph, seed);
    println!("planned buildings: {}", buildings.len());

    // Whole map.
    let mut canvas = Canvas::new(Vec2::ZERO, WORLD_SIZE, px_per_m);
    draw_world(&mut canvas, &graph, &buildings, px_per_m);
    match canvas.save(&out) {
        Ok(()) => println!(
            "world map saved to {out} ({}x{}, {px_per_m} px = 1 m)",
            canvas.size, canvas.size
        ),
        Err(err) => {
            eprintln!("failed to write {out}: {err}");
            std::process::exit(1);
        }
    }

    // Close-ups: one per subdivided type, centred on its densest district.
    if make_crops {
        let stem = out.strip_suffix(".png").unwrap_or(&out).to_string();
        for (suffix, kind) in [
            ("village", BlockType::UrbanVillage),
            ("oldcity", BlockType::OldCity),
            ("apartment", BlockType::Apartment),
        ] {
            let Some(center) = densest_center(&graph, kind) else {
                continue;
            };
            let origin = center - Vec2::splat(CROP_WORLD_WIDTH * 0.5);
            let mut crop = Canvas::new(origin, CROP_WORLD_WIDTH, CROP_PX_PER_M);
            draw_world(&mut crop, &graph, &buildings, CROP_PX_PER_M);
            let path = format!("{stem}_{suffix}.png");
            match crop.save(&path) {
                Ok(()) => println!(
                    "close-up saved to {path} ({CROP_WORLD_WIDTH:.0} m wide at {CROP_PX_PER_M} px/m, centre {:?})",
                    center.round()
                ),
                Err(err) => eprintln!("failed to write {path}: {err}"),
            }
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
}
