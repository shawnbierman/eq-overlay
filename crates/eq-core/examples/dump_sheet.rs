//! Render one full icon sheet upscaled with a red grid every 40px, so we can
//! visually locate a known icon (e.g. the mez swirl) and confirm the cell layout.
//!
//! Usage: cargo run -p eq-core --example dump_sheet -- <out.png> <icon_dir> <prefix> <sheet_num>

use image::{imageops, Rgba};

const ICON_PX: u32 = 40;
const SCALE: u32 = 4;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 4 {
        eprintln!("usage: dump_sheet <out.png> <icon_dir> <prefix> <sheet_num>");
        std::process::exit(2);
    }
    let (out, dir, prefix) = (&a[0], &a[1], &a[2]);
    let sheet: u32 = a[3].parse().expect("sheet num");
    // Optional grid cell size in source px (arg 5). 0 = no grid. Default 40.
    let grid: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(ICON_PX);
    let path = format!("{dir}/{prefix}{sheet:02}.tga");
    let img = image::open(&path).expect("open sheet").to_rgba8();
    let (w, h) = img.dimensions();
    println!("{path}: {w}x{h}  (grid {grid}px => {} cols x {} rows)",
        if grid > 0 { w / grid } else { 0 }, if grid > 0 { h / grid } else { 0 });

    let mut big = imageops::resize(&img, w * SCALE, h * SCALE, imageops::FilterType::Nearest);
    if grid > 0 {
        let step = grid * SCALE;
        let line = Rgba([255, 40, 40, 255]);
        let mut gx = 0u32;
        while gx < big.width() {
            for y in 0..big.height() {
                big.put_pixel(gx, y, line);
            }
            gx += step;
        }
        let mut gy = 0u32;
        while gy < big.height() {
            for x in 0..big.width() {
                big.put_pixel(x, gy, line);
            }
            gy += step;
        }
    }
    big.save(out).expect("save");
    println!("wrote {out}");
}
