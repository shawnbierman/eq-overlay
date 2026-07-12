//! Extract EQ spell icons from a `<prefix>NN.tga` sheet set and lay them out in
//! one horizontal PNG strip, so we can eyeball which icon set (classic
//! `gemicons` vs modern `Spells`) matches the in-game UI.
//!
//! Usage:
//!   cargo run -p eq-core --example dump_icons -- <out.png> <icon_dir> <prefix> <idx>...
//!
//! Sheets are a 6x6 grid of 40px icons (36 per sheet), so for icon `idx`:
//!   sheet = idx/36 + 1,  cell = idx%36,  col = cell%6,  row = cell/6.

use image::{imageops, Rgba, RgbaImage};

const ICONS_PER_SHEET: u32 = 36;
const ICON_PX: u32 = 40;
const SCALE: u32 = 3; // upscale (nearest) so icons are easy to see
const GAP: u32 = 6;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: dump_icons <out.png> <icon_dir> <prefix> <idx>...");
        std::process::exit(2);
    }
    let out = &args[0];
    let icon_dir = &args[1];
    let prefix = &args[2];
    let idxs: Vec<u32> = args[3..]
        .iter()
        .map(|s| s.parse().expect("idx must be a number"))
        .collect();

    let cell_px = ICON_PX * SCALE;
    let n = idxs.len() as u32;
    let mut canvas = RgbaImage::from_pixel(
        n * cell_px + (n + 1) * GAP,
        cell_px + 2 * GAP,
        Rgba([28, 30, 36, 255]), // dark backdrop so transparent icons read clearly
    );

    for (i, &idx) in idxs.iter().enumerate() {
        let sheet = idx / ICONS_PER_SHEET + 1;
        let path = format!("{icon_dir}/{prefix}{sheet:02}.tga");
        let img = match image::open(&path) {
            Ok(m) => m.to_rgba8(),
            Err(e) => {
                eprintln!("skip idx {idx}: {path}: {e}");
                continue;
            }
        };
        let cell = idx % ICONS_PER_SHEET;
        let (x0, y0) = ((cell % 6) * ICON_PX, (cell / 6) * ICON_PX);
        if x0 + ICON_PX > img.width() || y0 + ICON_PX > img.height() {
            eprintln!("skip idx {idx}: cell {cell} out of bounds on {path}");
            continue;
        }
        let crop = imageops::crop_imm(&img, x0, y0, ICON_PX, ICON_PX).to_image();
        let big = imageops::resize(&crop, cell_px, cell_px, imageops::FilterType::Nearest);
        let dx = (GAP + i as u32 * (cell_px + GAP)) as i64;
        imageops::overlay(&mut canvas, &big, dx, GAP as i64);
        println!("idx {idx}: {prefix}{sheet:02} cell {cell} (col {}, row {})", cell % 6, cell / 6);
    }

    canvas.save(out).expect("save png");
    println!("wrote {out} ({} icons of {prefix})", idxs.len());
}
