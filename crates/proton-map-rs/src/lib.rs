//! An offline vector basemap: a PMTiles archive of Mapbox Vector Tiles built
//! from Natural Earth and GeoNames, drawn into anti-aliased raster tiles.
//!
//! Nothing here touches the network. An app ships (or downloads once, by its
//! own means) a `.pmtiles` file built by the workspace's `basemap` tool,
//! opens it with [`Basemap`], and asks [`render_tile`] for the tiles it
//! shows, keeping them in a [`TileCache`].
//!
//! ```no_run
//! use proton_map::{Basemap, Style, TileCache, TileId, render_tile};
//!
//! let basemap = Basemap::open("basemap.pmtiles")?;
//! let cache = TileCache::new(64 << 20);
//! let style = Style::dark();
//! let tile = cache.get_or_render(TileId::new(4, 8, 5), 2.0, &style, |id, scale| {
//!     render_tile(&basemap, id, scale, &style)
//! });
//! assert_eq!(tile.width, 512);
//! # Ok::<(), proton_map::Error>(())
//! ```

mod cache;
pub mod labels;
pub mod mvt;
pub mod pmtiles;
mod render;
mod style;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use cache::{CacheKey, TileCache};
pub use render::{TILE_SIZE, Tile, render_tile};
pub use style::{Rgba, Style};

/// The credit a map drawn from the basemap must show.
pub const ATTRIBUTION: &str = "Map data © Natural Earth (public domain) and GeoNames (CC BY 4.0)";

/// Layer names, tag keys and values of the basemap's vector tiles, shared by
/// the builder and the renderer.
pub mod schema {
    /// Sea deeper than a band's `depth`, in metres.
    pub const BATHYMETRY: &str = "bathymetry";
    pub const DEPTH: &str = "depth";
    /// The depth bands, shallowest first.
    pub const DEPTHS: [u32; 7] = [200, 1000, 2000, 3000, 4000, 5000, 6000];
    /// Land; its outline is the coast.
    pub const LAND: &str = "land";
    pub const LAKES: &str = "lakes";
    pub const URBAN: &str = "urban";
    /// Rivers, tagged with their Natural Earth scale `rank` (0 largest).
    pub const RIVERS: &str = "rivers";
    pub const RANK: &str = "rank";
    pub const COUNTRIES: &str = "countries";
    /// State and province borders. Each part is its own feature tagged with
    /// its `distance` along the whole border at its start, in logical pixels
    /// at the tile's zoom, so dashes line up across tiles.
    pub const STATES: &str = "states";
    pub const DISTANCE: &str = "distance";
    /// Labelled places: `name`, size `class` (0-3) and label `side` (0 right
    /// of the dot, 1 left). Every place in the layer is drawn.
    pub const PLACES: &str = "places";
    pub const NAME: &str = "name";
    pub const CLASS: &str = "class";
    pub const SIDE: &str = "side";
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("reading the basemap: {0}")]
    Io(#[from] std::io::Error),
    #[error("broken basemap: {0}")]
    Format(&'static str),
}

/// A tile of the Web Mercator pyramid (XYZ scheme, y down from the north).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub const fn new(z: u8, x: u32, y: u32) -> Self {
        Self { z, x, y }
    }

    /// The tile at `zoom` holding the point at `latitude`, `longitude`.
    pub fn containing(latitude: f64, longitude: f64, zoom: u8) -> Self {
        let (x, y) = project(latitude, longitude);
        let n = (1u64 << zoom) as f64;
        let max = (1u64 << zoom) - 1;
        Self::new(
            zoom,
            ((x * n).floor() as u64).min(max) as u32,
            ((y * n).floor() as u64).min(max) as u32,
        )
    }

    /// The tile one zoom out that contains this one.
    pub fn parent(self) -> Option<Self> {
        (self.z > 0).then(|| Self::new(self.z - 1, self.x / 2, self.y / 2))
    }
}

/// Web Mercator: a position as x, y in 0..1 across the world map, x east
/// from 180° W and y south from about 85.05° N.
pub fn project(latitude: f64, longitude: f64) -> (f64, f64) {
    let lat = latitude.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let x = (longitude + 180.0) / 360.0;
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0;
    (x, y)
}

/// The latitude and longitude at Web Mercator `x`, `y`; the inverse of
/// [`project`].
pub fn unproject(x: f64, y: f64) -> (f64, f64) {
    let longitude = x * 360.0 - 180.0;
    let latitude = (std::f64::consts::PI * (1.0 - 2.0 * y))
        .sinh()
        .atan()
        .to_degrees();
    (latitude, longitude)
}

/// Decoded tiles kept by a [`Basemap`]; over-zoomed tiles draw from the same
/// source tile many times.
const DECODED_TILES: usize = 64;

/// The decoded layers of one tile.
type Layers = Arc<Vec<mvt::Layer>>;

/// Decoded tiles with the tick they were last used at.
type DecodedTiles = HashMap<TileId, (u64, Layers)>;

/// An opened basemap archive.
pub struct Basemap {
    archive: pmtiles::PmTiles,
    /// Layers the archive stops holding before its deepest zoom.
    layer_max_zoom: HashMap<String, u8>,
    decoded: Mutex<(u64, DecodedTiles)>,
}

impl std::fmt::Debug for Basemap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Basemap")
            .field("archive", &self.archive)
            .finish_non_exhaustive()
    }
}

impl Basemap {
    /// Open the archive at `path`, reading it into memory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::from_bytes(std::fs::read(path)?)
    }

    /// Open an archive from its bytes.
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Result<Self, Error> {
        let archive = pmtiles::PmTiles::new(bytes)?;
        if archive.header().tile_type != 1 {
            return Err(Error::Format("the archive doesn't hold vector tiles"));
        }
        let layer_max_zoom = archive
            .metadata()
            .map(|json| parse_layer_max_zoom(&json))
            .unwrap_or_default();
        Ok(Self {
            archive,
            layer_max_zoom,
            decoded: Mutex::new((0, HashMap::new())),
        })
    }

    pub fn archive(&self) -> &pmtiles::PmTiles {
        &self.archive
    }

    /// The deepest zoom the archive holds; deeper tiles are magnified.
    pub fn max_zoom(&self) -> u8 {
        self.archive.header().max_zoom
    }

    /// The deepest zoom the archive holds layer `name` at.
    pub fn layer_max_zoom(&self, name: &str) -> u8 {
        let max = self.max_zoom();
        self.layer_max_zoom.get(name).map_or(max, |&z| z.min(max))
    }

    /// The decoded layers of `tile`, `None` if the archive has no such tile
    /// or it doesn't decode.
    pub fn layers(&self, tile: TileId) -> Option<Arc<Vec<mvt::Layer>>> {
        {
            let mut guard = self.decoded.lock().unwrap_or_else(|e| e.into_inner());
            let (clock, map) = &mut *guard;
            *clock += 1;
            if let Some((used, layers)) = map.get_mut(&tile) {
                *used = *clock;
                return Some(layers.clone());
            }
        }
        let bytes = self.archive.tile(tile).ok()??;
        let layers = Arc::new(mvt::decode(&bytes).ok()?);
        let mut guard = self.decoded.lock().unwrap_or_else(|e| e.into_inner());
        let (clock, map) = &mut *guard;
        if map.len() >= DECODED_TILES
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(k, _)| *k)
        {
            map.remove(&oldest);
        }
        map.insert(tile, (*clock, layers.clone()));
        Some(layers)
    }
}

/// The metadata key naming layers that stop before the deepest zoom.
pub const LAYER_MAX_ZOOM_KEY: &str = "proton_layer_maxzoom";

/// Read `"proton_layer_maxzoom":{"layer":7,...}` from the metadata JSON. The
/// builder writes it flat, so a scan is enough; anything else is ignored.
fn parse_layer_max_zoom(json: &str) -> HashMap<String, u8> {
    let mut out = HashMap::new();
    let key = format!("\"{LAYER_MAX_ZOOM_KEY}\"");
    let Some(at) = json.find(&key) else {
        return out;
    };
    let rest = &json[at + key.len()..];
    let Some(open) = rest.find('{') else {
        return out;
    };
    let Some(close) = rest.find('}') else {
        return out;
    };
    if close < open {
        return out;
    }
    for pair in rest[open + 1..close].split(',') {
        let Some((name, zoom)) = pair.split_once(':') else {
            continue;
        };
        let name = name.trim().trim_matches('"');
        if let Ok(zoom) = zoom.trim().parse() {
            out.insert(name.to_owned(), zoom);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_zoom_limits_parse() {
        let json =
            r#"{"name":"x","proton_layer_maxzoom":{"bathymetry":6, "urban":8},"vector_layers":[]}"#;
        let limits = parse_layer_max_zoom(json);
        assert_eq!(limits.get("bathymetry"), Some(&6));
        assert_eq!(limits.get("urban"), Some(&8));
        assert!(parse_layer_max_zoom("{}").is_empty());
    }

    #[test]
    fn projection_round_trips() {
        for (lat, lon) in [(0.0, 0.0), (51.0, 6.0), (-33.9, 151.2), (85.0, -179.9)] {
            let (x, y) = project(lat, lon);
            let (lat2, lon2) = unproject(x, y);
            assert!((lat - lat2).abs() < 1e-9 && (lon - lon2).abs() < 1e-9);
        }
        assert_eq!(TileId::containing(51.0, 6.0, 7), TileId::new(7, 66, 42));
        assert_eq!(TileId::containing(90.0, 180.0, 2), TileId::new(2, 3, 0));
    }
}
