# proton-map-rs

An offline vector basemap for the Proton app family: a
[PMTiles v3](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md)
archive of [Mapbox Vector Tiles](https://github.com/mapbox/vector-tile-spec),
drawn into anti-aliased raster tiles with [tiny-skia](https://crates.io/crates/tiny-skia).

The crate has no network code. An app ships or fetches the `.pmtiles` file by
its own means, opens it, and renders the 256-point tiles it shows at the
screen's scale.

```rust,no_run
use proton_map::{Basemap, Style, TileCache, TileId, render_tile};

let basemap = Basemap::open("basemap.pmtiles")?;
let cache = TileCache::new(64 << 20); // 64 MiB of pixels
let style = Style::dark();
let tile = cache.get_or_render(TileId::new(7, 66, 42), 2.0, &style, |id, scale| {
    render_tile(&basemap, id, scale, &style)
});
// tile.width == tile.height == 512; tile.rgba is premultiplied RGBA.
# Ok::<(), proton_map::Error>(())
```

## What it draws

- Sea in eight shades by depth (Natural Earth bathymetry), land with a hairline
  coast, built-up areas, lakes, rivers widening with the zoom, country borders
  and dashed state borders.
- Town names in Inter Medium with a halo. The basemap builder chooses the
  labels of each zoom over the whole world (most important first, no two
  overlapping, every label of a zoom kept at the next), so tiles never
  disagree and a name cut by a tile edge continues in the next tile.
- `Style::light()` and `Style::dark()`: muted violet palettes. Every colour and
  width is a public field.
- Zooms beyond the archive's deepest level magnify the deepest tile, with lines
  and labels at their normal size. Layers can stop early (the archive's
  metadata says where) and are then drawn from the ancestor tile the same way.

## Pieces

| Module | What it holds |
|---|---|
| `pmtiles` | Header, directories, varints, Hilbert tile ids, gzip; with feature `writer`, a clustered archive writer with leaf directories, de-duplicated contents and run-length entries |
| `mvt` | A small hand-written protobuf decoder for vector tiles; with `writer`, an encoder |
| `labels` | Label layout, greedy collision-free selection, glyph outlines |
| crate root | `Basemap`, `render_tile`, `Style`, `TileCache` (thread-safe LRU by byte budget), Web Mercator helpers, `ATTRIBUTION` |

## Building the basemap

The archive is built by the workspace's `basemap` tool from Natural Earth
1:10m shapefiles and the GeoNames town list:

```sh
cargo run --release -p basemap -- --data ne-data --download --max-zoom 10 \
    --out basemap.pmtiles
```

`--download` fetches the missing source zips into `--data` with `curl`
(about 45 MB). `--cities` points at the town list, by default
`crates/proton-gallery-rs/data/cities15000.tsv`; GeoNames' own dump only adds
population and capital status. With zoom 10 the archive is about 25 MB and
builds in about 10 s on 8 cores.

To look at the result:

```sh
cargo run --release -p proton-map-rs --example render_png -- \
    basemap.pmtiles 7 51.0 6.0 dark 2 map.png
cargo run --release -p proton-map-rs --example render_bench -- basemap.pmtiles 2
```

## Licences

- Code: MIT.
- `fonts/Inter-Medium.ttf`: Inter by the Inter Project Authors, SIL Open Font
  License 1.1 (`fonts/Inter-OFL.txt`).
- Map data in a built basemap: Natural Earth (public domain) and GeoNames
  (CC BY 4.0). Show [`ATTRIBUTION`](https://docs.rs/proton-map-rs) with the map.
