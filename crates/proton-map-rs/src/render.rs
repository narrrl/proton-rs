//! Drawing one raster tile from the vector tiles of a [`Basemap`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform,
};

use crate::labels::{self, Side};
use crate::mvt::{Feature, Kind, Layer};
use crate::schema;
use crate::{Basemap, Style, TileId};

/// The size of a tile at scale 1, in pixels.
pub const TILE_SIZE: u32 = 256;

/// A rendered tile.
#[derive(Clone, PartialEq, Eq)]
pub struct Tile {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA, row by row, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Tile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tile")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Tile {
    /// The colour at a pixel as premultiplied RGBA.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let at = ((y * self.width + x) * 4) as usize;
        self.rgba[at..at + 4].try_into().expect("in bounds")
    }
}

fn paint(color: crate::Rgba) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color.skia());
    paint.anti_alias = true;
    paint
}

/// Everything needed to draw one tile's features into the pixmap.
struct Painter<'a> {
    pixmap: Pixmap,
    /// Tile units of the source tile to pixels.
    transform: Transform,
    /// The part of the source tile this tile shows, in tile units, with a
    /// margin of a few pixels: x0, y0, x1, y1.
    view: [f32; 4],
    /// Pixels per logical pixel.
    scale: f32,
    style: &'a Style,
}

/// Clip a closed ring to `view` (Sutherland-Hodgman). Where it leaves the
/// view the result runs along the view's edge, outside the tile.
fn clip_ring(ring: &[(f32, f32)], view: [f32; 4], out: &mut Vec<(f32, f32)>) {
    let mut points = ring.to_vec();
    for (edge, &bound) in view.iter().enumerate() {
        if points.is_empty() {
            break;
        }
        let inside = |p: (f32, f32)| match edge {
            0 => p.0 >= bound,
            1 => p.1 >= bound,
            2 => p.0 <= bound,
            _ => p.1 <= bound,
        };
        let cross = |p: (f32, f32), q: (f32, f32)| {
            if edge % 2 == 0 {
                let t = (bound - p.0) / (q.0 - p.0);
                (bound, p.1 + t * (q.1 - p.1))
            } else {
                let t = (bound - p.1) / (q.1 - p.1);
                (p.0 + t * (q.0 - p.0), bound)
            }
        };
        out.clear();
        let mut previous = *points.last().expect("non-empty");
        for &p in &points {
            match (inside(previous), inside(p)) {
                (true, true) => out.push(p),
                (true, false) => out.push(cross(previous, p)),
                (false, true) => {
                    out.push(cross(previous, p));
                    out.push(p);
                }
                (false, false) => {}
            }
            previous = p;
        }
        std::mem::swap(&mut points, out);
    }
    std::mem::swap(&mut points, out);
}

fn bounds(part: &[(f32, f32)]) -> [f32; 4] {
    let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for &(x, y) in part {
        b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
    }
    b
}

impl Painter<'_> {
    /// Add the parts of `feature` that reach into the view to `builder`;
    /// polygon rings are cut to the view, so a magnified tile doesn't
    /// rasterize all of its source tile.
    fn add_parts(&self, builder: &mut PathBuilder, feature: &Feature, polygon: bool) {
        let v = self.view;
        let mut clipped = Vec::new();
        for part in &feature.parts {
            if part.len() < 2 {
                continue;
            }
            let b = bounds(part);
            if b[2] < v[0] || b[3] < v[1] || b[0] > v[2] || b[1] > v[3] {
                continue;
            }
            let inside = b[0] >= v[0] && b[1] >= v[1] && b[2] <= v[2] && b[3] <= v[3];
            let points = if polygon && !inside {
                clip_ring(part, v, &mut clipped);
                if clipped.len() < 3 {
                    continue;
                }
                &clipped[..]
            } else {
                &part[..]
            };
            builder.move_to(points[0].0, points[0].1);
            for &(x, y) in &points[1..] {
                builder.line_to(x, y);
            }
            if polygon {
                builder.close();
            }
        }
    }

    fn fill(
        &mut self,
        layer: &Layer,
        color: crate::Rgba,
        anti_alias: bool,
        filter: impl Fn(&Feature) -> bool,
    ) {
        let mut paint = paint(color);
        paint.anti_alias = anti_alias;
        // One path per layer: overlapping features don't double their alpha.
        let mut builder = PathBuilder::new();
        for feature in layer
            .features
            .iter()
            .filter(|f| f.kind == Kind::Polygon && filter(f))
        {
            self.add_parts(&mut builder, feature, true);
        }
        if let Some(path) = builder.finish() {
            self.pixmap
                .fill_path(&path, &paint, FillRule::EvenOdd, self.transform, None);
        }
    }

    /// Stroke the shoreline of the land polygons in `layer`: their edges,
    /// except those the tile cut runs along (outside the tile, in its
    /// buffer) and those along the edge of the world, where the data cuts
    /// land at the antimeridian and the map's south end.
    fn coast(&mut self, layer: &Layer, width: f32, edges: WorldEdges) {
        let extent = layer.extent as f32;
        let v = self.view;
        let cut = |a: (f32, f32), b: (f32, f32)| {
            (a.0 < v[0] && b.0 < v[0])
                || (a.0 > v[2] && b.0 > v[2])
                || (a.1 < v[1] && b.1 < v[1])
                || (a.1 > v[3] && b.1 > v[3])
                || (edges.west && a.0 <= 0.0 && b.0 <= 0.0)
                || (edges.east && a.0 >= extent && b.0 >= extent)
                || (edges.north && a.1 <= 0.0 && b.1 <= 0.0)
                || (edges.south && a.1 >= extent && b.1 >= extent)
        };
        let mut builder = PathBuilder::new();
        for feature in layer.features.iter().filter(|f| f.kind == Kind::Polygon) {
            for ring in &feature.parts {
                let mut open = false;
                for i in 0..ring.len() {
                    let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
                    if cut(a, b) {
                        open = false;
                        continue;
                    }
                    if !open {
                        builder.move_to(a.0, a.1);
                        open = true;
                    }
                    builder.line_to(b.0, b.1);
                }
            }
        }
        let Some(path) = builder.finish() else {
            return;
        };
        let Some(path) = path.transform(self.transform) else {
            return;
        };
        // A hairline at most: the coast only sharpens the land's edge.
        self.stroke_pixels(&path, self.style.coast, (width * self.scale).min(1.0));
    }

    /// Stroke the features `filter` picks `width` logical pixels wide. `dash`
    /// gives a feature's dash pattern in device pixels.
    fn stroke(
        &mut self,
        layer: &Layer,
        color: crate::Rgba,
        width: f32,
        filter: impl Fn(&Feature) -> bool,
        dash: impl Fn(&Feature) -> Option<StrokeDash>,
    ) {
        for feature in layer
            .features
            .iter()
            .filter(|f| f.kind == Kind::Line && filter(f))
        {
            let mut builder = PathBuilder::new();
            self.add_parts(&mut builder, feature, false);
            // Stroke in pixel space so widths don't scale with the over-zoom.
            let Some(path) = builder.finish().and_then(|p| p.transform(self.transform)) else {
                continue;
            };
            // One stroke per feature: tiny-skia rasterizes a stroke within
            // its bounds, so one path of all features costs more.
            let path = match dash(feature) {
                Some(dash) => match path.dash(&dash, 1.0) {
                    Some(path) => path,
                    None => continue,
                },
                None => path,
            };
            self.stroke_pixels(&path, color, width * self.scale);
        }
    }

    /// Stroke a path already in pixels, `width` pixels wide. Lines of a
    /// pixel or less take tiny-skia's fast hairline path.
    fn stroke_pixels(&mut self, path: &tiny_skia::Path, color: crate::Rgba, width: f32) {
        // Round joins only show on lines wider than about two pixels. Square
        // caps under that: borders and rivers come in pieces, and pieces
        // ending flush leave a faint notch where both edges are blended.
        let (line_cap, line_join) = if width > 2.0 {
            (LineCap::Round, LineJoin::Round)
        } else {
            (LineCap::Square, LineJoin::Bevel)
        };
        let stroke = Stroke {
            width,
            line_cap,
            line_join,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(path, &paint(color), &stroke, Transform::identity(), None);
    }
}

/// Render `tile` at `scale` (device pixels per logical pixel; tiles are
/// 256 logical pixels square).
///
/// Each layer is drawn from the deepest zoom the basemap holds it at: a tile
/// deeper than that draws the matching part of its ancestor magnified, with
/// lines and labels at their normal size. So zooms beyond the archive's
/// deepest level still work, and layers whose detail runs out early (depth
/// bands, built-up areas) don't have to be stored at every zoom. Where the
/// archive has nothing the tile is open sea.
pub fn render_tile(basemap: &Basemap, tile: TileId, scale: f32, style: &Style) -> Tile {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let size = (TILE_SIZE as f32 * scale).round().max(1.0) as u32;
    let mut pixmap = Pixmap::new(size, size).expect("non-zero size");
    pixmap.fill(style.water[0].skia());
    let n = 1u64 << tile.z.min(31);
    if u64::from(tile.y) >= n {
        return Tile {
            width: size,
            height: size,
            rgba: pixmap.take(),
        };
    }
    let tile = TileId::new(tile.z, (u64::from(tile.x) % n) as u32, tile.y);

    // The source tiles by zoom, fetched once each.
    let mut sources: Vec<(u8, Option<Arc<Vec<Layer>>>)> = Vec::new();
    let mut painter = Painter {
        pixmap,
        transform: Transform::identity(),
        view: [0.0; 4],
        scale,
        style,
    };

    for name in DRAW_ORDER {
        let source_zoom = tile.z.min(basemap.layer_max_zoom(name));
        let dz = tile.z - source_zoom;
        let layers = match sources.iter().find(|(z, _)| *z == source_zoom) {
            Some((_, layers)) => layers.clone(),
            None => {
                let source = TileId::new(source_zoom, tile.x >> dz, tile.y >> dz);
                let layers = basemap.layers(source);
                sources.push((source_zoom, layers.clone()));
                layers
            }
        };
        let Some(layer) = layers
            .as_ref()
            .and_then(|layers| layers.iter().find(|layer| layer.name == name))
        else {
            continue;
        };
        let zoom_factor = (1u64 << dz) as f32;
        let mask = (1u32 << dz) - 1;
        let (sub_x, sub_y) = ((tile.x & mask) as f32, (tile.y & mask) as f32);
        let px_per_unit = size as f32 / layer.extent as f32 * zoom_factor;
        // Tile units per pixel of this tile; keep 4 px around it.
        let unit = layer.extent as f32 / size as f32 / zoom_factor;
        let span = layer.extent as f32 / zoom_factor;
        let margin = 4.0 * scale * unit;
        painter.view = [
            sub_x * span - margin,
            sub_y * span - margin,
            (sub_x + 1.0) * span + margin,
            (sub_y + 1.0) * span + margin,
        ];
        painter.transform = Transform::from_row(
            px_per_unit,
            0.0,
            0.0,
            px_per_unit,
            -sub_x * size as f32,
            -sub_y * size as f32,
        );
        let source = TileId::new(source_zoom, tile.x >> dz, tile.y >> dz);
        let last = (1u32 << source_zoom) - 1;
        let edges = WorldEdges {
            west: source.x == 0,
            north: source.y == 0,
            east: source.x == last,
            south: source.y == last,
        };
        draw_layer(&mut painter, layer, tile.z, zoom_factor, edges);
    }
    Tile {
        width: size,
        height: size,
        rgba: painter.pixmap.take(),
    }
}

/// Which edges of the world map a source tile touches.
#[derive(Debug, Clone, Copy)]
struct WorldEdges {
    west: bool,
    north: bool,
    east: bool,
    south: bool,
}

/// The layers, bottom to top.
const DRAW_ORDER: [&str; 8] = [
    schema::BATHYMETRY,
    schema::LAND,
    schema::URBAN,
    schema::LAKES,
    schema::RIVERS,
    schema::STATES,
    schema::COUNTRIES,
    schema::PLACES,
];

fn draw_layer(
    painter: &mut Painter<'_>,
    layer: &Layer,
    zoom: u8,
    zoom_factor: f32,
    edges: WorldEdges,
) {
    let style = painter.style;
    let scale = painter.scale;
    match layer.name.as_str() {
        schema::BATHYMETRY => {
            for (band, depth) in schema::DEPTHS.iter().enumerate() {
                let color = style.water[band + 1];
                // The bands differ by a few shades: their edges need no
                // anti-aliasing, which saves most of a tile's fill time.
                painter.fill(layer, color, false, |f| {
                    layer.number(f, schema::DEPTH) == Some(f64::from(*depth))
                });
            }
        }
        schema::LAND => {
            painter.fill(layer, style.land, true, |_| true);
            let width = style.coast_width * if zoom >= 6 { 1.25 } else { 1.0 };
            painter.coast(layer, width, edges);
        }
        schema::URBAN => {
            // Natural Earth's built-up areas are coarse; magnified they turn
            // into blocky shapes, so they fade as the map zooms in.
            let alpha = match zoom {
                0..=7 => 1.0,
                8 => 0.75,
                9 => 0.55,
                _ => 0.4,
            };
            let color = style
                .urban
                .with_alpha((f32::from(style.urban.a) * alpha) as u8);
            painter.fill(layer, color, true, |_| true);
        }
        schema::LAKES => painter.fill(layer, style.lake, true, |_| true),
        schema::RIVERS => {
            // One pass per rank, which sets the width.
            let mut ranks: Vec<i64> = layer
                .features
                .iter()
                .map(|f| layer.number(f, schema::RANK).unwrap_or(10.0) as i64)
                .collect();
            ranks.sort_unstable();
            ranks.dedup();
            for rank in ranks {
                painter.stroke(
                    layer,
                    style.river,
                    river_width(rank as f32, zoom),
                    |f| layer.number(f, schema::RANK).unwrap_or(10.0) as i64 == rank,
                    |_| None,
                );
            }
        }
        schema::STATES => {
            let [on, off] = style.state_dash;
            let (on, off) = (on * scale, off * scale);
            // Hairline dashes; thicker ones crowd the map.
            let width = style.state_border_width.min(1.0 / scale);
            painter.stroke(
                layer,
                style.state_border,
                width,
                |_| true,
                |feature| {
                    // Start the dash pattern where the border's own pattern
                    // is at this piece's start, so dashes run on across
                    // tiles.
                    let start = layer.number(feature, schema::DISTANCE).unwrap_or(0.0)
                        * f64::from(zoom_factor)
                        * f64::from(scale);
                    let phase = (start % f64::from(on + off)) as f32;
                    StrokeDash::new(vec![on, off], phase)
                },
            );
        }
        schema::COUNTRIES => {
            painter.stroke(layer, style.border, style.border_width, |_| true, |_| None);
        }
        schema::PLACES => draw_places(painter, layer),
        _ => {}
    }
}

/// Stroke width of a river of Natural Earth scale rank `rank` (0 is the
/// largest) at `zoom`.
fn river_width(rank: f32, zoom: u8) -> f32 {
    let base = (1.6 - rank * 0.12).max(0.5);
    let grow = match zoom {
        0..=4 => 0.7,
        5..=6 => 1.0,
        7..=8 => 1.4,
        9 => 2.0,
        10 => 2.5,
        11..=12 => 3.2,
        _ => 4.0,
    };
    base * grow
}

fn draw_places(painter: &mut Painter<'_>, places: &Layer) {
    let style = painter.style;
    let scale = painter.scale;
    let size = painter.pixmap.width() as i32;
    for feature in &places.features {
        let Some(name) = places.text(feature, schema::NAME) else {
            continue;
        };
        let class = places.number(feature, schema::CLASS).unwrap_or(3.0) as u8;
        let side = Side::from_number(places.number(feature, schema::SIDE).unwrap_or(0.0));
        let Some(&(x, y)) = feature.parts.first().and_then(|part| part.first()) else {
            continue;
        };
        let mut anchor = tiny_skia::Point::from_xy(x, y);
        painter
            .transform
            .map_points(std::slice::from_mut(&mut anchor));
        let Some(sprite) = label_sprite(name, class, side, scale, style) else {
            continue;
        };
        // Whole pixels: a label cut by a tile edge lands on the same pixels
        // in both tiles, as their offsets differ by a whole tile.
        let left = anchor.x.round() as i32 - sprite.anchor.0;
        let top = anchor.y.round() as i32 - sprite.anchor.1;
        let (w, h) = (sprite.pixmap.width() as i32, sprite.pixmap.height() as i32);
        if left >= size || top >= size || left + w <= 0 || top + h <= 0 {
            continue;
        }
        painter.pixmap.draw_pixmap(
            left,
            top,
            sprite.pixmap.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
}

/// A label drawn once: dot, halo and name.
struct Sprite {
    pixmap: Pixmap,
    /// Where the place's position is within the sprite, in pixels.
    anchor: (i32, i32),
}

#[derive(PartialEq, Eq, Hash)]
struct SpriteKey {
    name: String,
    class: u8,
    side: Side,
    scale: u32,
    colors: [crate::Rgba; 4],
    halo: u32,
}

/// Labels drawn so far; most places come back in every tile around them
/// and at every zoom.
const SPRITE_CACHE: usize = 4096;

fn label_sprite(
    name: &str,
    class: u8,
    side: Side,
    scale: f32,
    style: &Style,
) -> Option<Arc<Sprite>> {
    static SPRITES: OnceLock<Mutex<HashMap<SpriteKey, Arc<Sprite>>>> = OnceLock::new();
    let key = SpriteKey {
        name: name.to_owned(),
        class,
        side,
        scale: scale.to_bits(),
        colors: [style.label, style.label_halo, style.dot, style.dot_ring],
        halo: style.halo_width.to_bits(),
    };
    let sprites = SPRITES.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(sprite) = sprites.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Some(sprite.clone());
    }
    let sprite = Arc::new(draw_sprite(name, class, side, scale, style)?);
    let mut sprites = sprites.lock().unwrap_or_else(|e| e.into_inner());
    if sprites.len() >= SPRITE_CACHE {
        sprites.clear();
    }
    sprites.insert(key, sprite.clone());
    Some(sprite)
}

fn draw_sprite(name: &str, class: u8, side: Side, scale: f32, style: &Style) -> Option<Sprite> {
    let (origin, rect) = labels::layout(name, class, side);
    let pad = style.halo_width + 2.0;
    let anchor = (
        ((pad - rect.x0) * scale).ceil() as i32,
        ((pad - rect.y0) * scale).ceil() as i32,
    );
    let width = (anchor.0 as f32 + (rect.x1 + pad) * scale).ceil() as u32;
    let height = (anchor.1 as f32 + (rect.y1 + pad) * scale).ceil() as u32;
    let mut pixmap = Pixmap::new(width, height)?;
    let (ax, ay) = (anchor.0 as f32, anchor.1 as f32);
    let text = Transform::from_row(scale, 0.0, 0.0, scale, ax, ay);
    let glyphs = labels::text_path(name, labels::text_size(class), origin, text);
    let id = Transform::identity();
    if let Some(path) = &glyphs {
        let stroke = Stroke {
            width: style.halo_width * scale,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(path, &paint(style.label_halo), &stroke, id, None);
    }
    let mut dot = PathBuilder::new();
    dot.push_circle(ax, ay, labels::dot_radius(class) * scale);
    if let Some(dot) = dot.finish() {
        let ring = Stroke {
            width: 1.5 * scale,
            ..Stroke::default()
        };
        pixmap.stroke_path(&dot, &paint(style.dot_ring), &ring, id, None);
        pixmap.fill_path(&dot, &paint(style.dot), FillRule::Winding, id, None);
    }
    if let Some(path) = &glyphs {
        pixmap.fill_path(path, &paint(style.label), FillRule::Winding, id, None);
    }
    Some(Sprite { pixmap, anchor })
}
