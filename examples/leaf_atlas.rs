//! Bakes the per-species leaf alpha masks and writes preview sheets (R3.10).
//!
//! Run with `cargo run --example leaf_atlas`. Produces, under the project's
//! draft area:
//!
//! - `leaf_<species>.png`  -- the mask exactly as the game will use it at full
//!   resolution: the leaf in the silhouette colour over a dark background.
//! - `leaf_sheet.png`      -- all species side by side at equal pixel height,
//!   with a species key. This is the sheet to judge shape from.
//! - `leaf_sheet_mips.png` -- one row per mip level with the game's alpha cutoff
//!   applied, i.e. what a distant crown actually contributes. A shape that only
//!   works at full resolution is exposed here.
//!
//! Output goes to `.tmp/leaf_atlas/` (already gitignored) until the shapes are
//! approved; nothing here touches the renderer.
//!
//! Encoder: plain RGBA PNG via stored deflate, so this needs no image crate --
//! the same approach `tree_gallery` uses, extended with an alpha channel.

use no_one_looks_up::world::leaf_shape::{
    ALPHA_CUTOFF, AlphaMask, LEAF_VARIANTS, bake_leaf_alpha, build_mip_chain, default_mip_radius,
    leaf_profiles,
};

/// Vertical resolution of each per-species mask.
const MASK_HEIGHT: u32 = 512;
/// Levels in the preview chain (level 0 is the raw shape).
const CHAIN_LEVELS: u32 = 6;
/// Sheet layout.
const CAPTION_H: u32 = 30;
const GAP: u32 = 24;
const MARGIN: u32 = 24;
/// Background of the sheet: dark, so the near-black leaf still reads.
const BG: [u8; 4] = [24, 26, 24, 255];
/// The shipped tree silhouette, sRGB.
const LEAF_RGB: [u8; 3] = [8, 10, 8];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = std::path::Path::new(".tmp/leaf_atlas");
    std::fs::create_dir_all(out_dir)?;

    let mut baked: Vec<(
        usize,
        &'static no_one_looks_up::world::leaf_shape::LeafProfile,
        AlphaMask,
    )> = Vec::new();
    let mut chains: Vec<Vec<AlphaMask>> = Vec::new();

    for (species_index, profile) in leaf_profiles().iter().enumerate() {
        for variant in 0..LEAF_VARIANTS {
            // Level 0 stays the exact shape; per-level dilation only kicks in
            // down the mip chain (see `build_mip_chain`).
            let raw = bake_leaf_alpha(profile, variant, MASK_HEIGHT);
            let chain = build_mip_chain(&raw, CHAIN_LEVELS, default_mip_radius);

            println!(
                "{:<10} v{variant} {:>4}x{:<4} opaque {:>5.1}%  visible {:>5.2}%",
                profile.species,
                raw.width,
                raw.height,
                raw.opaque_fraction() * 100.0,
                raw.visible_fraction() * 100.0,
            );
            if variant == 0 {
                for (level, m) in chain.iter().enumerate().skip(1) {
                    println!(
                        "  mip{level}: {:>3}x{:<3} visible {:>6.3}%  (undilated {:>6.3}%)",
                        m.width,
                        m.height,
                        m.visible_fraction() * 100.0,
                        visible_after(&raw, level as u32) * 100.0,
                    );
                }
            }

            // Per-variant file: the level the player actually sees.
            let single = render_mask(&raw, BG, LEAF_RGB);
            write_png_rgba(
                &out_dir.join(format!("leaf_{}_v{variant}.png", profile.species)),
                raw.width,
                raw.height,
                &single,
            )?;

            baked.push((species_index, profile, raw));
            chains.push(chain);
        }
    }

    // Equal-height contact sheet so shapes are directly comparable.
    let total_h = MASK_HEIGHT + CAPTION_H + MARGIN * 2;
    let total_w = MARGIN * 2
        + baked.iter().map(|(_, _, m)| m.width).sum::<u32>()
        + GAP * (baked.len().saturating_sub(1) as u32);
    let mut sheet = vec![0u8; (total_w * total_h * 4) as usize];
    fill(&mut sheet, total_w, total_h, BG);

    let mut x = MARGIN;
    for (species_index, _, mask) in baked.iter() {
        // Centre each mask in a MASK_HEIGHT-tall slot.
        let y = MARGIN + CAPTION_H + (MASK_HEIGHT - mask.height) / 2;
        blit(&mut sheet, total_w, total_h, mask, x, y, BG, LEAF_RGB);

        // Species key: N filled bars above the leaf, N = table index + 1, so all
        // four variants of one species share their bars. A pixel font is not
        // worth writing for three labels; the names go to stdout in the same
        // order.
        let bar = 7;
        for i in 0..=*species_index {
            let bx = x + (i as u32) * (bar + 3);
            if bx + bar < total_w {
                for yy in MARGIN + 6..MARGIN + CAPTION_H - 6 {
                    for xx in bx..bx + bar {
                        set(&mut sheet, total_w, total_h, xx, yy, [200, 200, 200, 255]);
                    }
                }
            }
        }
        x += mask.width + GAP;
    }
    write_png_rgba(&out_dir.join("leaf_sheet.png"), total_w, total_h, &sheet)?;

    // Mip sheet: what the GPU keeps at 1/2, 1/4, 1/8 with the real cutoff
    // applied. One row per level; each mask is blown back up with nearest
    // neighbour so every row is the same pixel height and the comparison is
    // about shape rather than about how many texels survived.
    let mip_levels: [usize; 3] = [1, 2, 3];
    let total_mip_h = MASK_HEIGHT * mip_levels.len() as u32 + MARGIN * 2 + CAPTION_H;
    let mut mip_sheet = vec![0u8; (total_w * total_mip_h * 4) as usize];
    fill(&mut mip_sheet, total_w, total_mip_h, BG);

    let mut x = MARGIN;
    for (index, chain) in chains.iter().enumerate() {
        let base_w = baked[index].2.width;
        for (row, level) in mip_levels.iter().enumerate() {
            let m = &chain[*level];
            let scale = 1u32 << *level as u32;
            let y = MARGIN + CAPTION_H + row as u32 * MASK_HEIGHT;
            for py in 0..m.height {
                for px in 0..m.width {
                    if (m.get(px, py) as f32) < ALPHA_CUTOFF * 255.0 {
                        continue;
                    }
                    let dx0 = x + px * scale;
                    let dy0 = y + py * scale;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            set(
                                &mut mip_sheet,
                                total_w,
                                total_mip_h,
                                dx0 + dx,
                                dy0 + dy,
                                [LEAF_RGB[0], LEAF_RGB[1], LEAF_RGB[2], 255],
                            );
                        }
                    }
                }
            }
        }
        x += base_w + GAP;
    }
    write_png_rgba(
        &out_dir.join("leaf_sheet_mips.png"),
        total_w,
        total_mip_h,
        &mip_sheet,
    )?;

    println!(
        "\nwrote {}/leaf_sheet.png and leaf_sheet_mips.png",
        out_dir.display()
    );
    println!("columns (left to right): four variants per species, in table order");
    for (i, (_, profile, _)) in baked.iter().enumerate() {
        println!(
            "  column {} -> {} ({})",
            i + 1,
            profile.label,
            profile.species
        );
    }
    Ok(())
}

/// Visible fraction after `level` halvings.
fn visible_after(mask: &AlphaMask, level: u32) -> f32 {
    let mut m = mask.clone();
    for _ in 0..level {
        m = m.downsample_half();
    }
    m.visible_fraction()
}

/// Renders a mask as RGBA over `bg`, composite so the file is viewable anywhere.
fn render_mask(mask: &AlphaMask, bg: [u8; 4], fg: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::with_capacity((mask.width * mask.height * 4) as usize);
    for y in 0..mask.height {
        for x in 0..mask.width {
            let a = mask.get(x, y) as f32 / 255.0;
            out.extend_from_slice(&[
                (fg[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8,
                (fg[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8,
                (fg[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8,
                255,
            ]);
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn blit(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    mask: &AlphaMask,
    ox: u32,
    oy: u32,
    bg: [u8; 4],
    fg: [u8; 3],
) {
    for y in 0..mask.height {
        for x in 0..mask.width {
            let a = mask.get(x, y) as f32 / 255.0;
            let px = [
                (fg[0] as f32 * a + bg[0] as f32 * (1.0 - a)) as u8,
                (fg[1] as f32 * a + bg[1] as f32 * (1.0 - a)) as u8,
                (fg[2] as f32 * a + bg[2] as f32 * (1.0 - a)) as u8,
                255,
            ];
            set(dst, dst_w, dst_h, ox + x, oy + y, px);
        }
    }
}

fn set(dst: &mut [u8], w: u32, h: u32, x: u32, y: u32, px: [u8; 4]) {
    if x >= w || y >= h {
        return;
    }
    let i = ((y * w + x) * 4) as usize;
    dst[i..i + 4].copy_from_slice(&px);
}

fn fill(dst: &mut [u8], w: u32, h: u32, px: [u8; 4]) {
    for y in 0..h {
        for x in 0..w {
            set(dst, w, h, x, y, px);
        }
    }
}

// --- minimal RGBA PNG encoder (stored deflate, no dependencies) ---

fn write_png_rgba(
    path: &std::path::Path,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        rgba.len(),
        (width * height * 4) as usize,
        "rgba size mismatch"
    );
    let mut raw = Vec::with_capacity(rgba.len() + height as usize);
    for y in 0..height as usize {
        raw.push(0u8);
        let row = y * width as usize * 4;
        raw.extend_from_slice(&rgba[row..row + width as usize * 4]);
    }
    let compressed = zlib_stored(&raw);

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // bit depth 8, colour type 6 (RGBA)
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
