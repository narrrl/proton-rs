# Changelog

All notable changes to proton-map-rs are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and the crate
follows semantic versioning.

## [0.1.0] - 2026-11-21

### Added

- `Basemap`: opens a PMTiles v3 archive of vector tiles from a file or bytes,
  with per-layer zoom limits read from its metadata.
- `render_tile`: draws a 256-point tile at any scale with tiny-skia, magnifying
  the deepest tile beyond the archive's zooms.
- `Style::light()` and `Style::dark()`, muted violet palettes with depth-shaded
  sea, built-up areas, rivers, borders and haloed town labels in Inter Medium.
- `TileCache`: a thread-safe least-recently-used cache of rendered tiles by
  byte budget.
- `pmtiles`, `mvt` and `labels` modules; with feature `writer`, a PMTiles
  writer and a vector tile encoder.
- `ATTRIBUTION` for Natural Earth and GeoNames.
