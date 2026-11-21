//! Builds the offline basemap of `proton-map-rs`: Natural Earth 1:10m
//! physical and cultural layers plus the GeoNames towns, cut into Mapbox
//! Vector Tiles and packed into one PMTiles archive.
//!
//! ```text
//! basemap --data <dir> [--download] [--cities <tsv>] [--max-zoom 10] --out basemap.pmtiles
//! ```
//!
//! `--data` holds the Natural Earth zips (`ne_10m_land.zip`, ...) and
//! GeoNames' `cities15000.zip`; with `--download` missing ones are fetched
//! with `curl`. `--cities` is the gallery's trimmed town list
//! (`crates/proton-gallery-rs/data/cities15000.tsv`), which decides which
//! towns exist and what they are called; GeoNames' dump only adds their
//! population and whether they are a capital.

mod geo;

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use proton_map::labels::{self, Candidate, Side};
use proton_map::mvt::Kind;
use proton_map::mvt::encode::{LayerBuilder, Tag, tile as encode_tile};
use proton_map::pmtiles::writer::{Writer, gzip};
use proton_map::{TileId, schema};
use rayon::prelude::*;

use geo::{BBox, Rings};

/// Geometry units across a tile.
const EXTENT: u32 = 4096;
/// Geometry around each tile, in pixels, so strokes and anti-aliasing at the
/// tile's edge match its neighbour.
const BUFFER_PX: f64 = 12.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Layer {
    Bathymetry,
    Land,
    Urban,
    Lakes,
    Rivers,
    States,
    Countries,
    Places,
}

const LAYERS: [Layer; 8] = [
    Layer::Bathymetry,
    Layer::Land,
    Layer::Urban,
    Layer::Lakes,
    Layer::Rivers,
    Layer::States,
    Layer::Countries,
    Layer::Places,
];

impl Layer {
    fn name(self) -> &'static str {
        match self {
            Layer::Bathymetry => schema::BATHYMETRY,
            Layer::Land => schema::LAND,
            Layer::Urban => schema::URBAN,
            Layer::Lakes => schema::LAKES,
            Layer::Rivers => schema::RIVERS,
            Layer::States => schema::STATES,
            Layer::Countries => schema::COUNTRIES,
            Layer::Places => schema::PLACES,
        }
    }

    /// The buffer around a tile this layer is clipped to, in pixels.
    fn buffer_px(self) -> f64 {
        match self {
            Layer::Places => f64::from(labels::LABEL_BUFFER_PX),
            _ => BUFFER_PX,
        }
    }

    /// The deepest zoom the layer is stored at; deeper tiles draw it from
    /// their ancestor at this zoom. Natural Earth's depth bands, built-up
    /// areas and state borders have no more detail to show beyond these.
    fn max_zoom(self, archive: u8) -> u8 {
        let limit = match self {
            Layer::Bathymetry => 5,
            Layer::Urban | Layer::States => 7,
            Layer::Rivers | Layer::Places => 8,
            Layer::Lakes | Layer::Countries => 9,
            _ => archive,
        };
        limit.min(archive)
    }

    /// The simplification tolerance, in pixels of the zoom being built.
    fn tolerance_px(self, deepest: bool) -> f64 {
        match self {
            Layer::Bathymetry => 1.5,
            Layer::Urban | Layer::States => 0.6,
            // Deeper tiles magnify the deepest ones, so they keep more.
            Layer::Land | Layer::Lakes | Layer::Rivers | Layer::Countries if deepest => 0.4,
            _ => 0.5,
        }
    }
}

type Tags = Arc<Vec<(&'static str, Tag)>>;

#[derive(Debug, Clone)]
enum Geom {
    Poly(Rings),
    Line(geo::Lines),
    Point([f64; 2]),
}

/// A feature as read, in world units.
#[derive(Debug)]
struct Source {
    layer: Layer,
    geom: SourceGeom,
    /// The first zoom the feature shows at.
    min_zoom: f64,
    tags: Tags,
}

#[derive(Debug)]
enum SourceGeom {
    Poly(Rings),
    Line(Vec<Vec<[f64; 2]>>),
}

/// A feature of one zoom being cut into tiles.
#[derive(Debug, Clone)]
struct Feat {
    layer: Layer,
    geom: Geom,
    tags: Tags,
}

struct Args {
    data: PathBuf,
    download: bool,
    cities: PathBuf,
    out: PathBuf,
    max_zoom: u8,
}

fn usage() -> ! {
    eprintln!(
        "usage: basemap --data <dir> [--download] [--cities <tsv>] [--max-zoom <z>] --out <file>"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut data = None;
    let mut out = None;
    let mut download = false;
    let mut cities = PathBuf::from("crates/proton-gallery-rs/data/cities15000.tsv");
    let mut max_zoom = 10;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data" => data = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--cities" => cities = args.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--max-zoom" => {
                max_zoom = args
                    .next()
                    .and_then(|z| z.parse().ok())
                    .filter(|z| *z <= 14)
                    .unwrap_or_else(|| usage())
            }
            "--download" => download = true,
            _ => usage(),
        }
    }
    let (Some(data), Some(out)) = (data, out) else {
        usage()
    };
    Args {
        data,
        download,
        cities,
        out,
        max_zoom,
    }
}

const NATURAL_EARTH: &str = "https://naciscdn.org/naturalearth/10m";
const GEONAMES: &str = "https://download.geonames.org/export/dump/cities15000.zip";

/// The zip `name` in the data directory, downloaded first if asked to.
fn source_file(args: &Args, name: &str, url: &str) -> PathBuf {
    let path = args.data.join(name);
    if !path.exists() {
        if !args.download {
            eprintln!("missing {}; pass --download to fetch it", path.display());
            std::process::exit(1);
        }
        std::fs::create_dir_all(&args.data).expect("creating the data directory");
        eprintln!("downloading {url}");
        let status = Command::new("curl")
            .args(["-sfL", "-o"])
            .arg(&path)
            .arg(url)
            .status()
            .expect("running curl");
        if !status.success() {
            let _ = std::fs::remove_file(&path);
            eprintln!("downloading {url} failed");
            std::process::exit(1);
        }
    }
    path
}

fn natural_earth(args: &Args, name: &str) -> PathBuf {
    let theme = if name.starts_with("admin") || name.starts_with("urban") {
        "cultural"
    } else {
        "physical"
    };
    source_file(
        args,
        &format!("ne_10m_{name}.zip"),
        &format!("{NATURAL_EARTH}/{theme}/ne_10m_{name}.zip"),
    )
}

fn zip_member(zip: &Path, suffix: &str) -> Vec<u8> {
    let file = std::fs::File::open(zip).expect("opening a source zip");
    let mut archive = zip::ZipArchive::new(file).expect("reading a source zip");
    let name = archive
        .file_names()
        .filter_map(Result::ok)
        .find(|n| n.ends_with(suffix))
        .map(|n| n.into_owned())
        .unwrap_or_else(|| panic!("{} has no {suffix}", zip.display()));
    let mut bytes = Vec::new();
    archive
        .by_name(&name)
        .expect("listed member")
        .read_to_end(&mut bytes)
        .expect("reading a zip member");
    bytes
}

type Record = shapefile::dbase::Record;

fn number(record: &Record, names: &[&str]) -> Option<f64> {
    use shapefile::dbase::FieldValue;
    names.iter().find_map(|name| match record.get(name)? {
        FieldValue::Numeric(Some(n)) | FieldValue::Double(n) => Some(*n),
        FieldValue::Float(Some(n)) => Some(f64::from(*n)),
        FieldValue::Integer(n) => Some(f64::from(*n)),
        _ => None,
    })
}

/// Every shape of the shapefile in `zip` with its attributes.
fn read_shapes(zip: &Path) -> Vec<(shapefile::Shape, Record)> {
    let shp = zip_member(zip, ".shp");
    let dbf = zip_member(zip, ".dbf");
    let shapes = shapefile::ShapeReader::new(Cursor::new(shp)).expect("a .shp");
    let records = shapefile::dbase::Reader::new(Cursor::new(dbf)).expect("a .dbf");
    let mut reader = shapefile::Reader::new(shapes, records);
    reader
        .iter_shapes_and_records()
        .map(|r| r.expect("a shape"))
        .collect()
}

fn project(point: &shapefile::Point) -> [f64; 2] {
    let (x, y) = proton_map::project(point.y, point.x);
    [x, y]
}

fn polygon_rings(shape: &shapefile::Shape) -> Rings {
    let rings = match shape {
        shapefile::Shape::Polygon(p) => p.rings(),
        _ => return Vec::new(),
    };
    rings
        .iter()
        .map(|ring| {
            let mut points: Vec<[f64; 2]> = ring.points().iter().map(project).collect();
            if points.len() > 1 && points.first() == points.last() {
                points.pop();
            }
            points
        })
        .filter(|ring| ring.len() >= 3)
        .collect()
}

fn polyline_parts(shape: &shapefile::Shape) -> Vec<Vec<[f64; 2]>> {
    match shape {
        shapefile::Shape::Polyline(p) => p
            .parts()
            .iter()
            .map(|part| part.iter().map(project).collect())
            .collect(),
        _ => Vec::new(),
    }
}

fn no_tags() -> Tags {
    Arc::new(Vec::new())
}

fn load_sources(args: &Args) -> Vec<Source> {
    let mut sources = Vec::new();

    for depth in schema::DEPTHS {
        let letter = match depth {
            200 => "K",
            1000 => "J",
            2000 => "I",
            3000 => "H",
            4000 => "G",
            5000 => "F",
            _ => "E",
        };
        let path = natural_earth(args, &format!("bathymetry_{letter}_{depth}"));
        let tags: Tags = Arc::new(vec![(schema::DEPTH, Tag::Int(i64::from(depth)))]);
        for (shape, _) in read_shapes(&path) {
            sources.push(Source {
                layer: Layer::Bathymetry,
                geom: SourceGeom::Poly(polygon_rings(&shape)),
                min_zoom: 0.0,
                tags: tags.clone(),
            });
        }
    }

    for (shape, _) in read_shapes(&natural_earth(args, "land")) {
        sources.push(Source {
            layer: Layer::Land,
            geom: SourceGeom::Poly(polygon_rings(&shape)),
            min_zoom: 0.0,
            tags: no_tags(),
        });
    }

    for (shape, record) in read_shapes(&natural_earth(args, "lakes")) {
        sources.push(Source {
            layer: Layer::Lakes,
            geom: SourceGeom::Poly(polygon_rings(&shape)),
            min_zoom: number(&record, &["min_zoom"]).unwrap_or(0.0) - 1.5,
            tags: no_tags(),
        });
    }

    for (shape, record) in read_shapes(&natural_earth(args, "urban_areas")) {
        sources.push(Source {
            layer: Layer::Urban,
            geom: SourceGeom::Poly(polygon_rings(&shape)),
            min_zoom: (number(&record, &["min_zoom"]).unwrap_or(8.0) - 2.0).max(5.0),
            tags: no_tags(),
        });
    }

    // A river comes in pieces with their own first zoom; it shows whole from
    // the zoom its first piece does, rather than as broken dashes.
    let rivers = read_shapes(&natural_earth(args, "rivers_lake_centerlines"));
    let river_id = |record: &Record| number(record, &["rivernum"]).map(|n| n as i64);
    let mut river_zoom: HashMap<i64, f64> = HashMap::new();
    for (_, record) in &rivers {
        if let (Some(id), Some(zoom)) = (river_id(record), number(record, &["min_zoom"])) {
            let entry = river_zoom.entry(id).or_insert(zoom);
            *entry = entry.min(zoom);
        }
    }
    for (shape, record) in &rivers {
        let rank = number(record, &["scalerank"]).unwrap_or(10.0);
        let own = number(record, &["min_zoom"]).unwrap_or(rank);
        let first = river_id(record).and_then(|id| river_zoom.get(&id).copied());
        sources.push(Source {
            layer: Layer::Rivers,
            geom: SourceGeom::Line(polyline_parts(shape)),
            min_zoom: first.unwrap_or(own) - 1.0,
            tags: Arc::new(vec![(schema::RANK, Tag::Int(rank as i64))]),
        });
    }

    // Every border from zoom 1: Natural Earth gives pieces of one border
    // different first zooms, which would draw it dashed.
    for (shape, _) in read_shapes(&natural_earth(args, "admin_0_boundary_lines_land")) {
        sources.push(Source {
            layer: Layer::Countries,
            geom: SourceGeom::Line(polyline_parts(&shape)),
            min_zoom: 1.0,
            tags: no_tags(),
        });
    }

    for (shape, record) in read_shapes(&natural_earth(args, "admin_1_states_provinces_lines")) {
        sources.push(Source {
            layer: Layer::States,
            geom: SourceGeom::Line(polyline_parts(&shape)),
            min_zoom: (number(&record, &["MIN_ZOOM", "min_zoom"]).unwrap_or(6.0) - 1.5).max(4.0),
            tags: no_tags(),
        });
    }
    sources
}

/// A town that may be labelled.
struct Place {
    name: String,
    x: f64,
    y: f64,
    population: u64,
    capital: bool,
    admin_seat: bool,
}

impl Place {
    fn importance(&self) -> f64 {
        let boost = if self.capital {
            4.0
        } else if self.admin_seat {
            1.3
        } else {
            1.0
        };
        self.population as f64 * boost
    }

    fn class(&self) -> u8 {
        if self.population >= 5_000_000 || (self.capital && self.population >= 1_000_000) {
            0
        } else if self.population >= 1_000_000 || self.capital {
            1
        } else if self.population >= 200_000 {
            2
        } else {
            3
        }
    }

    /// Whether the town is big enough to be considered at `zoom`.
    fn shows_at(&self, zoom: u8) -> bool {
        let p = self.population;
        match zoom {
            0 => false,
            1 => p >= 15_000_000 || (self.capital && p >= 5_000_000),
            2 => p >= 5_000_000 || (self.capital && p >= 1_000_000),
            3 => p >= 2_000_000 || self.capital,
            4 => p >= 700_000 || self.capital,
            5 => p >= 250_000 || self.capital,
            6 => p >= 100_000 || self.capital || (self.admin_seat && p >= 50_000),
            7 => p >= 50_000 || self.admin_seat,
            8 => p >= 25_000 || self.admin_seat,
            _ => true,
        }
    }
}

fn load_places(args: &Args) -> Vec<Place> {
    let geonames = source_file(args, "cities15000.zip", GEONAMES);
    let dump = String::from_utf8(zip_member(&geonames, "cities15000.txt")).expect("UTF-8 dump");
    let mut facts: HashMap<u32, (u64, String)> = HashMap::new();
    for line in dump.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 15 {
            continue;
        }
        if let (Ok(id), Ok(pop)) = (cols[0].parse(), cols[14].parse()) {
            facts.insert(id, (pop, cols[7].to_owned()));
        }
    }
    let table = std::fs::read_to_string(&args.cities).expect("reading the town list");
    let mut places: Vec<Place> = table
        .lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id: u32 = cols.next()?.parse().ok()?;
            let name = cols.next()?.to_owned();
            let lat: f64 = cols.next()?.parse().ok()?;
            let lon: f64 = cols.next()?.parse().ok()?;
            let (population, code) = facts.get(&id).cloned().unwrap_or((15_000, String::new()));
            let (x, y) = proton_map::project(lat, lon);
            Some(Place {
                name,
                x,
                y,
                population,
                capital: code == "PPLC",
                admin_seat: code == "PPLA",
            })
        })
        .collect();
    places.sort_by(|a, b| b.importance().total_cmp(&a.importance()));
    places
}

/// The labels of each zoom: place index and side. A zoom keeps every label
/// of the one before.
fn choose_labels(places: &[Place], max_zoom: u8) -> Vec<Vec<(usize, Side)>> {
    let mut by_zoom: Vec<Vec<(usize, Side)>> = Vec::new();
    let mut previous: Vec<(usize, Side)> = Vec::new();
    for z in 0..=max_zoom {
        let world = 256.0 * (1u64 << z) as f64;
        let mut order: Vec<(usize, Option<Side>)> =
            previous.iter().map(|&(i, s)| (i, Some(s))).collect();
        let kept: std::collections::HashSet<usize> = previous.iter().map(|&(i, _)| i).collect();
        order.extend(
            places
                .iter()
                .enumerate()
                .filter(|(i, p)| !kept.contains(i) && p.shows_at(z))
                .map(|(i, _)| (i, None)),
        );
        let candidates: Vec<Candidate> = order
            .iter()
            .map(|&(i, side)| Candidate {
                x: (places[i].x * world) as f32,
                y: (places[i].y * world) as f32,
                name: places[i].name.clone(),
                class: places[i].class(),
                side,
            })
            .collect();
        let world_rect = labels::Rect {
            x0: 0.0,
            y0: 0.0,
            x1: world as f32,
            y1: world as f32,
        };
        let chosen: Vec<(usize, Side)> = labels::select(&candidates, Some(world_rect))
            .into_iter()
            .map(|placed| (order[placed.index].0, placed.side))
            .collect();
        by_zoom.push(chosen.clone());
        previous = chosen;
    }
    by_zoom
}

/// The features of zoom `z`, simplified for it.
fn features_at(
    sources: &[Source],
    places: &[Place],
    labels: &[(usize, Side)],
    z: u8,
    max_zoom: u8,
) -> Vec<Feat> {
    let px = 1.0 / (256.0 * (1u64 << z) as f64);
    let mut feats: Vec<Feat> = sources
        .par_iter()
        .filter(|s| s.min_zoom <= f64::from(z) && z <= s.layer.max_zoom(max_zoom))
        .filter_map(|s| {
            let tolerance = s.layer.tolerance_px(z == s.layer.max_zoom(max_zoom)) * px;
            let geom = match &s.geom {
                SourceGeom::Poly(rings) => {
                    let rings: Rings = rings
                        .iter()
                        .filter_map(|ring| {
                            let b = geo::BBox::of_points(ring)?;
                            if (b.x1 - b.x0).max(b.y1 - b.y0) < px {
                                return None;
                            }
                            let ring = geo::simplify_ring(ring, tolerance);
                            (ring.len() >= 3 && geo::area2(&ring).abs() / 2.0 >= px * px)
                                .then_some(ring)
                        })
                        .collect();
                    if rings.is_empty() {
                        return None;
                    }
                    Geom::Poly(rings)
                }
                SourceGeom::Line(parts) => {
                    let parts: geo::Lines = parts
                        .iter()
                        .filter_map(|part| {
                            // Short pieces stay: borders and rivers come in
                            // pieces, and a dropped one leaves a gap.
                            let line = geo::with_distance(&geo::simplify(part, tolerance));
                            (line.len() >= 2).then_some(line)
                        })
                        .collect();
                    if parts.is_empty() {
                        return None;
                    }
                    Geom::Line(parts)
                }
            };
            Some(Feat {
                layer: s.layer,
                geom,
                tags: s.tags.clone(),
            })
        })
        .collect();
    let labels = if z <= Layer::Places.max_zoom(max_zoom) {
        labels
    } else {
        &[]
    };
    for &(index, side) in labels {
        let place = &places[index];
        feats.push(Feat {
            layer: Layer::Places,
            geom: Geom::Point([place.x, place.y]),
            tags: Arc::new(vec![
                (schema::NAME, Tag::Text(place.name.clone())),
                (schema::CLASS, Tag::Int(i64::from(place.class()))),
                (schema::SIDE, Tag::Int(side as i64)),
            ]),
        });
    }
    feats
}

fn tile_box(tile: TileId) -> BBox {
    let n = (1u64 << tile.z) as f64;
    BBox {
        x0: f64::from(tile.x) / n,
        y0: f64::from(tile.y) / n,
        x1: f64::from(tile.x + 1) / n,
        y1: f64::from(tile.y + 1) / n,
    }
}

fn feat_bbox(geom: &Geom) -> Option<BBox> {
    match geom {
        Geom::Poly(rings) => BBox::of_points(rings.iter().flatten()),
        Geom::Line(lines) => {
            let mut points = lines.iter().flatten().map(|p| [p[0], p[1]]);
            let first = points.next()?;
            let mut b = BBox::of_points([&first])?;
            for p in points {
                b.x0 = b.x0.min(p[0]);
                b.y0 = b.y0.min(p[1]);
                b.x1 = b.x1.max(p[0]);
                b.y1 = b.y1.max(p[1]);
            }
            Some(b)
        }
        Geom::Point(p) => BBox::of_points([p]),
    }
}

/// `feats` clipped to `tile`, each with its layer's buffer at `zoom`.
fn clip(feats: &[Feat], tile: TileId, zoom: u8) -> Vec<Feat> {
    let px = 1.0 / (256.0 * (1u64 << zoom) as f64);
    let base = tile_box(tile);
    feats
        .iter()
        .filter_map(|f| {
            let b = base.expand(f.layer.buffer_px() * px);
            let fb = feat_bbox(&f.geom)?;
            if fb.disjoint(&b) {
                return None;
            }
            if fb.inside(&b) {
                return Some(f.clone());
            }
            let geom = match &f.geom {
                Geom::Poly(rings) => {
                    let rings: Rings = rings
                        .iter()
                        .map(|r| geo::clip_ring(r, &b))
                        .filter(|r| r.len() >= 3 && geo::area2(r).abs() > 0.0)
                        .collect();
                    if rings.is_empty() {
                        return None;
                    }
                    Geom::Poly(rings)
                }
                Geom::Line(lines) => {
                    let lines = geo::clip_lines(lines, &b);
                    if lines.is_empty() {
                        return None;
                    }
                    Geom::Line(lines)
                }
                Geom::Point(_) => return None,
            };
            Some(Feat {
                layer: f.layer,
                geom,
                tags: f.tags.clone(),
            })
        })
        .collect()
}

/// Whether every feature is a polygon covering the whole clipped box, so all
/// tiles below look the same.
fn uniform(feats: &[Feat], tile: TileId, zoom: u8) -> bool {
    let px = 1.0 / (256.0 * (1u64 << zoom) as f64);
    feats.iter().all(|f| {
        let Geom::Poly(rings) = &f.geom else {
            return false;
        };
        let b = tile_box(tile).expand(f.layer.buffer_px() * px);
        let [ring] = &rings[..] else {
            return false;
        };
        let full = (b.x1 - b.x0) * (b.y1 - b.y0);
        let eps = full * 1e-9;
        BBox::of_points(ring).is_some_and(|rb| {
            (rb.x0 - b.x0).abs() < 1e-12
                && (rb.y0 - b.y0).abs() < 1e-12
                && (rb.x1 - b.x1).abs() < 1e-12
                && (rb.y1 - b.y1).abs() < 1e-12
        }) && (geo::area2(ring).abs() / 2.0 - full).abs() < eps
    })
}

/// The gzipped vector tile of `tile` holding `feats` (already clipped).
fn encode(feats: &[Feat], tile: TileId) -> Vec<u8> {
    let n = (1u64 << tile.z) as f64;
    let e = f64::from(EXTENT);
    let q = |x: f64, y: f64| -> (i32, i32) {
        (
            ((x * n - f64::from(tile.x)) * e).round() as i32,
            ((y * n - f64::from(tile.y)) * e).round() as i32,
        )
    };
    let mut builders: Vec<LayerBuilder> = LAYERS
        .iter()
        .map(|l| LayerBuilder::new(l.name(), EXTENT))
        .collect();
    for f in feats {
        let builder = &mut builders[LAYERS.iter().position(|l| *l == f.layer).expect("listed")];
        let tags: Vec<(&str, Tag)> = f.tags.iter().map(|(k, v)| (*k, v.clone())).collect();
        match &f.geom {
            Geom::Poly(rings) => {
                let rings: Vec<Vec<(i32, i32)>> = rings
                    .iter()
                    .map(|r| {
                        let mut out: Vec<(i32, i32)> = Vec::with_capacity(r.len());
                        for p in r {
                            let p = q(p[0], p[1]);
                            if out.last() != Some(&p) {
                                out.push(p);
                            }
                        }
                        while out.len() > 1 && out.first() == out.last() {
                            out.pop();
                        }
                        out
                    })
                    .filter(|r| {
                        let ring: Vec<[f64; 2]> = r
                            .iter()
                            .map(|&(x, y)| [f64::from(x), f64::from(y)])
                            .collect();
                        r.len() >= 3 && geo::area2(&ring) != 0.0
                    })
                    .collect();
                builder.add(Kind::Polygon, &rings, &tags);
            }
            Geom::Line(lines) => {
                for line in lines {
                    let mut out: Vec<(i32, i32)> = Vec::with_capacity(line.len());
                    for p in line {
                        let p = q(p[0], p[1]);
                        if out.last() != Some(&p) {
                            out.push(p);
                        }
                    }
                    if out.len() < 2 {
                        continue;
                    }
                    if f.layer == Layer::States {
                        let mut tags = tags.clone();
                        let distance = line[0][2] * 256.0 * n;
                        tags.push((schema::DISTANCE, Tag::Double(distance.round())));
                        builder.add(Kind::Line, &[out], &tags);
                    } else {
                        builder.add(Kind::Line, &[out], &tags);
                    }
                }
            }
            Geom::Point(p) => {
                builder.add(Kind::Point, &[vec![q(p[0], p[1])]], &tags);
            }
        }
    }
    gzip(&encode_tile(&builders))
}

/// Cut `feats` (clipped to `node`) into the tiles of zoom `zoom` below it.
fn cut(feats: Vec<Feat>, node: TileId, zoom: u8) -> Vec<(TileId, Vec<u8>)> {
    if feats.is_empty() {
        return Vec::new();
    }
    if node.z == zoom {
        return vec![(node, encode(&feats, node))];
    }
    if uniform(&feats, node, zoom) {
        let dz = zoom - node.z;
        let first = TileId::new(zoom, node.x << dz, node.y << dz);
        let bytes = encode(&clip(&feats, first, zoom), first);
        let side = 1u32 << dz;
        let mut out = Vec::with_capacity((side * side) as usize);
        for x in 0..side {
            for y in 0..side {
                out.push((
                    TileId::new(zoom, (node.x << dz) + x, (node.y << dz) + y),
                    bytes.clone(),
                ));
            }
        }
        return out;
    }
    let children = [(0, 0), (1, 0), (0, 1), (1, 1)]
        .map(|(dx, dy)| TileId::new(node.z + 1, node.x * 2 + dx, node.y * 2 + dy));
    children
        .into_par_iter()
        .flat_map(|child| cut(clip(&feats, child, zoom), child, zoom))
        .collect()
}

fn metadata(max_zoom: u8) -> String {
    let layers: Vec<String> = LAYERS
        .iter()
        .map(|l| format!(r#"{{"id":"{}","fields":{{}}}}"#, l.name()))
        .collect();
    let limits: Vec<String> = LAYERS
        .iter()
        .filter(|l| l.max_zoom(max_zoom) < max_zoom)
        .map(|l| format!(r#""{}":{}"#, l.name(), l.max_zoom(max_zoom)))
        .collect();
    format!(
        r#"{{"name":"proton-map basemap","format":"pbf","type":"baselayer","minzoom":"0","maxzoom":"{max_zoom}","attribution":"{}","{}":{{{}}},"vector_layers":[{}]}}"#,
        proton_map::ATTRIBUTION,
        proton_map::LAYER_MAX_ZOOM_KEY,
        limits.join(","),
        layers.join(",")
    )
}

fn main() {
    let args = parse_args();
    let started = Instant::now();
    let sources = load_sources(&args);
    let places = load_places(&args);
    eprintln!(
        "read {} features and {} towns in {:.1} s",
        sources.len(),
        places.len(),
        started.elapsed().as_secs_f64()
    );
    let chosen = choose_labels(&places, args.max_zoom);

    let mut writer = Writer::new();
    for z in 0..=args.max_zoom {
        let at = Instant::now();
        let feats = features_at(&sources, &places, &chosen[z as usize], z, args.max_zoom);
        let root = TileId::new(0, 0, 0);
        let tiles = cut(clip(&feats, root, z), root, z);
        let bytes: usize = tiles.iter().map(|(_, b)| b.len()).sum();
        eprintln!(
            "z{z}: {} tiles, {} labels, {:.1} MB before dedup, {:.1} s",
            tiles.len(),
            chosen[z as usize].len(),
            bytes as f64 / 1e6,
            at.elapsed().as_secs_f64()
        );
        for (tile, bytes) in tiles {
            writer.add(tile, bytes);
        }
    }
    let archive = writer.finish(
        &metadata(args.max_zoom),
        [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
        [0.0, 20.0],
    );
    std::fs::write(&args.out, &archive).expect("writing the archive");
    eprintln!(
        "wrote {} ({:.1} MB) in {:.1} s",
        args.out.display(),
        archive.len() as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
}
