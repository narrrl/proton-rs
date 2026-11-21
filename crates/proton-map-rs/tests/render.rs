//! Rendering a basemap built in the test with the crate's own writer.

use proton_map::mvt::Kind;
use proton_map::mvt::encode::{LayerBuilder, Tag, tile};
use proton_map::pmtiles::writer::{Writer, gzip};
use proton_map::{Basemap, Rgba, Style, TileId, render_tile, schema};

const EXTENT: u32 = 4096;

/// One z2 tile: land on its western half, a deep-sea band in the
/// south-east, a border, a river and one labelled town.
fn fixture() -> Basemap {
    let mut land = LayerBuilder::new(schema::LAND, EXTENT);
    land.add(
        Kind::Polygon,
        &[vec![(-64, -64), (2048, -64), (2048, 4160), (-64, 4160)]],
        &[],
    );
    let mut sea = LayerBuilder::new(schema::BATHYMETRY, EXTENT);
    sea.add(
        Kind::Polygon,
        &[vec![(3072, 3072), (4160, 3072), (4160, 4160), (3072, 4160)]],
        &[(schema::DEPTH, Tag::Int(6000))],
    );
    let mut countries = LayerBuilder::new(schema::COUNTRIES, EXTENT);
    countries.add(Kind::Line, &[vec![(0, 1024), (2048, 1024)]], &[]);
    let mut rivers = LayerBuilder::new(schema::RIVERS, EXTENT);
    rivers.add(
        Kind::Line,
        &[vec![(512, 0), (512, 4096)]],
        &[(schema::RANK, Tag::Int(1))],
    );
    let mut places = LayerBuilder::new(schema::PLACES, EXTENT);
    places.add(
        Kind::Point,
        &[vec![(1024, 2048)]],
        &[
            (schema::NAME, Tag::Text("Testville".into())),
            (schema::CLASS, Tag::Int(0)),
            (schema::SIDE, Tag::Int(0)),
        ],
    );
    let bytes = tile(&[sea, land, rivers, countries, places]);
    let mut writer = Writer::new();
    writer.add(TileId::new(2, 1, 1), gzip(&bytes));
    let archive = writer.finish(
        r#"{"proton_layer_maxzoom":{"bathymetry":2}}"#,
        [-180.0, -85.0, 180.0, 85.0],
        [0.0, 0.0],
    );
    Basemap::from_bytes(archive).unwrap()
}

/// `c` as an opaque premultiplied pixel.
fn opaque(c: Rgba) -> [u8; 4] {
    [c.r, c.g, c.b, 255]
}

fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 2)
}

#[test]
fn land_and_sea_land_where_they_belong() {
    let basemap = fixture();
    for style in [Style::light(), Style::dark()] {
        let tile = render_tile(&basemap, TileId::new(2, 1, 1), 2.0, &style);
        assert_eq!((tile.width, tile.height), (512, 512));
        assert_eq!(tile.rgba.len(), 512 * 512 * 4);
        // West: land, away from the river, border and label.
        assert!(close(tile.pixel(200, 450), opaque(style.land)));
        // North-east: shallow sea; south-east: the deepest band.
        assert!(close(tile.pixel(450, 60), opaque(style.water[0])));
        assert!(close(tile.pixel(480, 480), opaque(style.water[7])));
        // The river, the border and the town's dot draw over the land.
        assert!(!close(tile.pixel(64, 400), opaque(style.land)));
        assert!(!close(tile.pixel(200, 128), opaque(style.land)));
        assert!(close(tile.pixel(128, 256), opaque(style.dot)));
        // The name to the right of the dot.
        let text =
            (134..240).any(|x| (240..262).any(|y| close(tile.pixel(x, y), opaque(style.label))));
        assert!(text, "no label text drawn");
    }
}

#[test]
fn deeper_zooms_magnify_the_deepest_tile() {
    let basemap = fixture();
    let style = Style::light();
    // z4 (4, 4) is the north-west sixteenth of z2 (1, 1): land, with the
    // river (source x = 512 of 4096) at 512 / 1024 of the tile, its middle.
    let tile = render_tile(&basemap, TileId::new(4, 4, 4), 1.0, &style);
    assert!(close(tile.pixel(40, 200), opaque(style.land)));
    assert!(!close(tile.pixel(128, 200), opaque(style.land)));
    // Far beyond the archive's zoom, still drawn.
    let deep = render_tile(&basemap, TileId::new(14, 4 << 10, 4 << 10), 1.0, &style);
    assert!(close(deep.pixel(10, 10), opaque(style.land)));
    // A tile the archive doesn't hold is open sea.
    let empty = render_tile(&basemap, TileId::new(2, 3, 0), 1.0, &style);
    assert!(
        empty
            .rgba
            .chunks(4)
            .all(|p| close(p.try_into().unwrap(), opaque(style.water[0])))
    );
}

#[test]
fn tiles_are_not_flat() {
    let basemap = fixture();
    let tile = render_tile(&basemap, TileId::new(2, 1, 1), 1.0, &Style::dark());
    let first = tile.pixel(0, 0);
    let distinct = tile.rgba.chunks(4).filter(|p| *p != first).count();
    assert!(distinct > 10_000, "{distinct}");
}
