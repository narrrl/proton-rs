//! Render a 3x3 mosaic of tiles around a position into a PNG, to look at.
//! The scale defaults to 2 (512 px tiles).
//!
//! ```text
//! cargo run --release -p proton-map-rs --example render_png -- \
//!     basemap.pmtiles <zoom> <lat> <lon> <light|dark> [scale] out.png
//! ```

use std::fs::File;
use std::io::BufWriter;

use proton_map::{Basemap, Style, TileId, render_tile};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        eprintln!(
            "usage: render_png <basemap.pmtiles> <zoom> <lat> <lon> <light|dark> [scale] <out.png>"
        );
        std::process::exit(2);
    }
    let basemap = Basemap::open(&args[0]).expect("opening the basemap");
    let zoom: u8 = args[1].parse().expect("zoom");
    let lat: f64 = args[2].parse().expect("latitude");
    let lon: f64 = args[3].parse().expect("longitude");
    let style = if args[4] == "dark" {
        Style::dark()
    } else {
        Style::light()
    };
    let (scale, out) = if args.len() >= 7 {
        (args[5].parse().expect("scale"), &args[6])
    } else {
        (2.0, &args[5])
    };
    let center = TileId::containing(lat, lon, zoom);
    // Three tiles square, or the whole world when it is smaller.
    let n = 1u32 << zoom;
    let grid = n.min(3);
    let first = |c: u32| c.saturating_sub(grid / 2).min(n - grid);
    let (x0, y0) = (first(center.x), first(center.y));
    let side = (256.0 * scale) as u32;
    let width = side * grid;
    let mut image = vec![0u8; (width * width * 4) as usize];
    for dy in 0..grid {
        for dx in 0..grid {
            let tile = render_tile(&basemap, TileId::new(zoom, x0 + dx, y0 + dy), scale, &style);
            for row in 0..tile.height {
                let src = (row * tile.width * 4) as usize;
                let dst = (((dy * side + row) * width + dx * side) * 4) as usize;
                image[dst..dst + (tile.width * 4) as usize]
                    .copy_from_slice(&tile.rgba[src..src + (tile.width * 4) as usize]);
            }
        }
    }
    let file = BufWriter::new(File::create(out).expect("creating the PNG"));
    let mut encoder = png::Encoder::new(file, width, width);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("PNG header");
    // The tiles are opaque, so premultiplied and straight alpha agree.
    writer.write_image_data(&image).expect("PNG data");
}
