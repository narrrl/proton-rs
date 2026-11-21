//! Time tile rendering: renders the tiles around a few cities at several
//! zooms and prints the milliseconds per tile.
//!
//! ```text
//! cargo run --release -p proton-map-rs --example render_bench -- basemap.pmtiles [scale]
//! ```

use std::time::Instant;

use proton_map::{Basemap, Style, TileId, render_tile};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: render_bench <basemap.pmtiles> [scale]");
        std::process::exit(2);
    };
    let scale: f32 = args.get(1).map_or(2.0, |s| s.parse().expect("scale"));
    let basemap = Basemap::open(path).expect("opening the basemap");
    let places = [
        (50.94, 6.96),
        (48.86, 2.35),
        (40.71, -74.0),
        (35.68, 139.69),
        (-33.87, 151.21),
        (60.0, 25.0),
    ];
    for style in [Style::light(), Style::dark()] {
        let mut tiles = Vec::new();
        for zoom in [2u8, 4, 6, 8, 10, 12] {
            for &(lat, lon) in &places {
                let c = TileId::containing(lat, lon, zoom);
                let n = 1u32 << zoom;
                for dx in 0..2 {
                    for dy in 0..2 {
                        tiles.push(TileId::new(zoom, (c.x + dx) % n, (c.y + dy).min(n - 1)));
                    }
                }
            }
        }
        // Warm the decoded-tile cache and the glyph cache once.
        for &tile in &tiles {
            render_tile(&basemap, tile, scale, &style);
        }
        let mut by_zoom: Vec<(u8, f64, usize)> = Vec::new();
        let started = Instant::now();
        for &tile in &tiles {
            let at = Instant::now();
            let out = render_tile(&basemap, tile, scale, &style);
            std::hint::black_box(&out);
            let ms = at.elapsed().as_secs_f64() * 1e3;
            match by_zoom.iter_mut().find(|(z, _, _)| *z == tile.z) {
                Some(entry) => {
                    entry.1 += ms;
                    entry.2 += 1;
                }
                None => by_zoom.push((tile.z, ms, 1)),
            }
        }
        let total = started.elapsed().as_secs_f64() * 1e3;
        let size = (256.0 * scale) as u32;
        println!(
            "style {}: {} tiles of {size}x{size} px, {:.2} ms per tile",
            style.id,
            tiles.len(),
            total / tiles.len() as f64
        );
        for (zoom, ms, count) in by_zoom {
            println!("  z{zoom:<2} {:.2} ms per tile", ms / count as f64);
        }
    }
}
