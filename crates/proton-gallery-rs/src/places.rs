//! Offline reverse geocoding: the town a photo was taken in, from its GPS
//! position, without asking any service.
//!
//! The towns are GeoNames' `cities15000` dump (every place with more than
//! 15,000 people), trimmed to id, name, position and country in
//! `data/cities15000.tsv` by `scripts/geonames-cities.py`. The script drops the
//! parts of a city, such as Kreuzberg or "Paris 15 Vaugirard", so that a photo
//! lands in the town people would name. GeoNames publishes the dump under
//! CC BY 4.0, which is why the About dialog credits it.
//!
//! `data/countries.tsv` maps each country code to its ISO 3166-1 English name,
//! as the `iso-codes` project spells it, so a front end can translate it
//! through that project's `iso_3166-1` gettext domain.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// How far from the nearest town a photo may be and still count as taken
/// there, in km. Farther out (at sea, in the wilderness) it has no place.
pub const MAX_DISTANCE_KM: f64 = 50.0;

/// Mean Earth radius in km, for the haversine distance.
const EARTH_RADIUS_KM: f64 = 6371.0;

const CITIES: &str = include_str!("../data/cities15000.tsv");
const COUNTRIES: &str = include_str!("../data/countries.tsv");

/// One town of the table.
#[derive(Debug, Clone, PartialEq)]
pub struct City {
    /// The GeoNames id, stable across dumps.
    pub id: u32,
    pub name: &'static str,
    pub latitude: f64,
    pub longitude: f64,
    /// ISO 3166-1 alpha-2 country code.
    pub country: &'static str,
}

/// The towns, and an index of them by whole-degree cell.
struct Table {
    cities: Vec<City>,
    cells: HashMap<(i32, i32), Vec<usize>>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let cities: Vec<City> = CITIES.lines().filter_map(parse_city).collect();
        let mut cells: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (index, city) in cities.iter().enumerate() {
            cells
                .entry(cell(city.latitude, city.longitude))
                .or_default()
                .push(index);
        }
        Table { cities, cells }
    })
}

fn parse_city(line: &'static str) -> Option<City> {
    let mut fields = line.split('\t');
    Some(City {
        id: fields.next()?.parse().ok()?,
        name: fields.next()?,
        latitude: fields.next()?.parse().ok()?,
        longitude: fields.next()?.parse().ok()?,
        country: fields.next()?,
    })
}

fn cell(latitude: f64, longitude: f64) -> (i32, i32) {
    (latitude.floor() as i32, longitude.floor() as i32)
}

/// Great-circle distance between two positions, in km.
pub fn distance_km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lat2) = (a.0.to_radians(), b.0.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.1 - a.1).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// The town nearest to a position, if one lies within [`MAX_DISTANCE_KM`].
pub fn nearest(latitude: f64, longitude: f64) -> Option<&'static City> {
    if !latitude.is_finite() || !longitude.is_finite() {
        return None;
    }
    let table = table();
    // A degree of latitude is about 111 km everywhere; a degree of longitude
    // shrinks towards the poles, so more cells are needed to cover the radius.
    let lat_span = (MAX_DISTANCE_KM / 111.0).ceil() as i32;
    let lon_span = {
        let scale = latitude.to_radians().cos().abs().max(0.01);
        ((MAX_DISTANCE_KM / (111.0 * scale)).ceil() as i32).min(180)
    };
    let (row, column) = cell(latitude, longitude);
    let mut best: Option<(f64, &City)> = None;
    for dlat in -lat_span..=lat_span {
        for dlon in -lon_span..=lon_span {
            // Longitude wraps at the antimeridian: -181 is 179.
            let lon_cell = (column + dlon + 180).rem_euclid(360) - 180;
            let Some(indices) = table.cells.get(&(row + dlat, lon_cell)) else {
                continue;
            };
            for &index in indices {
                let city = &table.cities[index];
                let d = distance_km((latitude, longitude), (city.latitude, city.longitude));
                if d <= MAX_DISTANCE_KM && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, city));
                }
            }
        }
    }
    best.map(|(_, city)| city)
}

/// The town with this GeoNames id.
pub fn city(id: u32) -> Option<&'static City> {
    let cities = &table().cities;
    // The table is sorted by id.
    cities
        .binary_search_by_key(&id, |city| city.id)
        .ok()
        .map(|index| &cities[index])
}

/// The English ISO 3166-1 name of a country code, such as "Germany" for "DE".
pub fn country_name(code: &str) -> Option<&'static str> {
    COUNTRIES.lines().find_map(|line| {
        let (c, name) = line.split_once('\t')?;
        (c == code).then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_in_a_town_resolves_to_that_town() {
        let berlin = nearest(52.52, 13.405).expect("Berlin is a town");
        assert_eq!(berlin.name, "Berlin");
        assert_eq!(berlin.country, "DE");
        assert_eq!(city(berlin.id), Some(berlin));
    }

    #[test]
    fn the_open_sea_has_no_town() {
        assert_eq!(nearest(0.0, -30.0), None);
        assert_eq!(nearest(f64::NAN, 0.0), None);
    }

    #[test]
    fn a_town_across_the_antimeridian_is_still_found() {
        // Just east of 180°, the nearest towns in Fiji are west of it.
        let town = nearest(-16.8, 179.99);
        assert!(town.is_none_or(|t| t.country == "FJ"));
        let suva = nearest(-18.14, 178.44).expect("Suva is a town");
        assert_eq!(suva.country, "FJ");
    }

    #[test]
    fn every_town_has_a_named_country() {
        for city in &table().cities {
            assert!(
                country_name(city.country).is_some(),
                "{} has no country name for {}",
                city.name,
                city.country
            );
        }
        assert_eq!(country_name("DE"), Some("Germany"));
    }

    #[test]
    fn distances_are_great_circle_km() {
        let d = distance_km((52.52, 13.405), (48.137, 11.575));
        assert!(
            (d - 504.0).abs() < 5.0,
            "Berlin to Munich is about 504 km, got {d}"
        );
    }
}

const LAND: &str = include_str!("../data/land.txt");
const BORDERS: &str = include_str!("../data/borders.txt");
const MAX_LATITUDE: f64 = 85.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct World {
    pub x: f64,
    pub y: f64,
}

pub fn project(latitude: f64, longitude: f64) -> World {
    let lat = latitude.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    World {
        x: (longitude + 180.0) / 360.0,
        y: (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0,
    }
}

/// One land outline or border line, projected, with its bounding box for
/// culling.
pub struct Ring {
    pub points: Vec<World>,
    pub min: World,
    pub max: World,
}

pub fn land() -> &'static [Ring] {
    static LAND_RINGS: OnceLock<Vec<Ring>> = OnceLock::new();
    LAND_RINGS.get_or_init(|| LAND.lines().filter_map(parse_ring).collect())
}

pub fn borders() -> &'static [Ring] {
    static BORDER_LINES: OnceLock<Vec<Ring>> = OnceLock::new();
    BORDER_LINES.get_or_init(|| BORDERS.lines().filter_map(parse_ring).collect())
}

fn parse_ring(line: &str) -> Option<Ring> {
    let points: Vec<World> = line
        .split(' ')
        .filter_map(|pair| {
            let (lon, lat) = pair.split_once(',')?;
            Some(project(lat.parse().ok()?, lon.parse().ok()?))
        })
        .collect();
    let first = *points.first()?;
    let (min, max) = points.iter().fold((first, first), |(min, max), p| {
        (
            World {
                x: min.x.min(p.x),
                y: min.y.min(p.y),
            },
            World {
                x: max.x.max(p.x),
                y: max.y.max(p.y),
            },
        )
    });
    Some(Ring { points, min, max })
}

/// Radius in px of a marker holding `photos` photos: big enough to make the
/// photo out, and growing with the count, but slowly, so a home town with
/// thousands doesn't cover a continent.
pub fn bubble_radius(photos: usize) -> f64 {
    18.0 + 2.0 * (photos.max(1) as f64).ln()
}

#[derive(Debug, Clone)]
pub struct Bubble {
    pub x: f64,
    pub y: f64,
    pub radius: f64,
    pub photos: usize,
    pub members: Vec<usize>,
}

/// Merge markers whose circles would overlap on screen, the one with the
/// most photos claiming its neighbours (the first of equals, when counts
/// tie). Markers off screen are left out.
pub fn cluster(screen: &[(f64, f64)], counts: &[usize], width: f64, height: f64) -> Vec<Bubble> {
    let mut order: Vec<usize> = (0..screen.len()).collect();
    order.sort_by(|a, b| counts[*b].cmp(&counts[*a]));
    let mut bubbles: Vec<Bubble> = Vec::new();
    for index in order {
        let (x, y) = screen[index];
        let reach = bubble_radius(counts[index]);
        if x < -reach || y < -reach || x > width + reach || y > height + reach {
            continue;
        }
        match bubbles
            .iter_mut()
            .find(|b| (b.x - x).hypot(b.y - y) < b.radius + reach * 0.5)
        {
            Some(bubble) => {
                bubble.photos += counts[index];
                bubble.radius = bubble_radius(bubble.photos);
                bubble.members.push(index);
            }
            None => bubbles.push(Bubble {
                x,
                y,
                radius: reach,
                photos: counts[index],
                members: vec![index],
            }),
        }
    }
    // Smallest last, so a small marker next to a big one stays clickable.
    bubbles.sort_by(|a, b| b.radius.total_cmp(&a.radius));
    bubbles
}

/// "7", "840", "1.2k", "35k": short enough to fit a marker. Digits and the
/// "k" read the same in every language the app ships.
pub fn compact_count(n: usize) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{}k", n / 1_000),
    }
}

/// Where a photo was taken, expressed in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coordinates {
    pub latitude: f64,
    pub longitude: f64,
}

impl Coordinates {
    pub fn new(latitude: f64, longitude: f64) -> Option<Self> {
        ((-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude)).then_some(
            Self {
                latitude,
                longitude,
            },
        )
    }

    pub fn projected(self) -> World {
        project(self.latitude, self.longitude)
    }
}

/// Validated EXIF GPS coordinates. Missing hemisphere or malformed DMS tags
/// must not place a marker on the opposite side of the globe.
pub fn exif_coordinates(exif: &exif::Exif) -> Option<Coordinates> {
    use exif::{In, Tag, Value};
    let coordinate = |tag, reference, positive, negative| {
        let Value::Rational(parts) = &exif.get_field(tag, In::PRIMARY)?.value else {
            return None;
        };
        let Value::Ascii(hemispheres) = &exif.get_field(reference, In::PRIMARY)?.value else {
            return None;
        };
        let sign = match hemispheres.first()?.first().copied()? {
            value if value == positive => 1.0,
            value if value == negative => -1.0,
            _ => return None,
        };
        let [degrees, minutes, seconds] = parts.as_slice() else {
            return None;
        };
        if parts.iter().any(|part| part.denom == 0)
            || minutes.to_f64() >= 60.0
            || seconds.to_f64() >= 60.0
        {
            return None;
        }
        Some(sign * (degrees.to_f64() + minutes.to_f64() / 60.0 + seconds.to_f64() / 3600.0))
    };
    Coordinates::new(
        coordinate(Tag::GPSLatitude, Tag::GPSLatitudeRef, b'N', b'S')?,
        coordinate(Tag::GPSLongitude, Tag::GPSLongitudeRef, b'E', b'W')?,
    )
}

/// Read location from a bounded prefix of the original image.
pub fn embedded_coordinates(bytes: &[u8]) -> Option<Coordinates> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    exif_coordinates(&exif)
}

use crate::Result;
use crate::catalog::Catalog;
use crate::model::AssetKey;
use rusqlite::{OptionalExtension, params};

impl Catalog {
    /// Recheck visibility for a batch even when its input outline predates a
    /// visibility change. Removed assets are excluded by the same lookup.
    pub async fn visible_location_keys(&self, keys: Vec<AssetKey>) -> Result<HashSet<AssetKey>> {
        if keys.is_empty() {
            return Ok(HashSet::new());
        }
        self.run(move |conn| {
            let mut statement = conn.prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM assets a WHERE (a.account,a.volume,a.node)=(?1,?2,?3) AND {})",
                crate::phone::not_hidden("a"),
            ))?;
            let mut visible = HashSet::new();
            for key in keys {
                if statement.query_row(
                    params![key.account.as_str(), key.volume.as_str(), key.node.as_str()],
                    |row| row.get::<_, bool>(0),
                )? {
                    visible.insert(key);
                }
            }
            Ok(visible)
        }).await
    }

    /// Look up the location of each current, visible asset. A present row with
    /// NULL coordinates records a completed scan of a photo without location.
    pub async fn photo_locations(
        &self,
        candidates: Vec<(AssetKey, Option<String>)>,
    ) -> Result<Vec<Option<Option<Coordinates>>>> {
        self.run(move |conn| {
            let mut statement = conn.prepare_cached(
                "SELECT latitude, longitude FROM photo_locations
                 WHERE (account,volume,node)=(?1,?2,?3) AND revision IS ?4",
            )?;
            candidates
                .into_iter()
                .map(|(key, revision)| {
                    statement
                        .query_row(
                            params![
                                key.account.as_str(),
                                key.volume.as_str(),
                                key.node.as_str(),
                                revision
                            ],
                            |row| {
                                Ok(row
                                    .get::<_, Option<f64>>(0)?
                                    .zip(row.get::<_, Option<f64>>(1)?)
                                    .and_then(|(latitude, longitude)| {
                                        Coordinates::new(latitude, longitude)
                                    }))
                            },
                        )
                        .optional()
                        .map_err(Into::into)
                })
                .collect()
        })
        .await
    }

    pub async fn store_photo_locations(
        &self,
        locations: Vec<(AssetKey, Option<String>, Option<Coordinates>)>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut statement = tx.prepare_cached(
                    "INSERT INTO photo_locations(account,volume,node,revision,latitude,longitude)
                     SELECT ?1,?2,?3,?4,?5,?6 FROM assets a
                     WHERE (a.account,a.volume,a.node)=(?1,?2,?3)
                         AND json_extract(a.payload,'$.revision') IS ?4
                         AND NOT EXISTS (
                             SELECT 1 FROM memberships m JOIN album_settings s ON s.source=m.source
                             WHERE (m.account,m.volume,m.node)=(?1,?2,?3) AND s.hidden=1
                         )
                     ON CONFLICT(account,volume,node) DO UPDATE SET
                        revision=excluded.revision, latitude=excluded.latitude, longitude=excluded.longitude",
                )?;
                for (key, revision, location) in locations {
                    statement.execute(params![
                        key.account.as_str(), key.volume.as_str(), key.node.as_str(), revision,
                        location.map(|p| p.latitude), location.map(|p| p.longitude)
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        }).await
    }
}

#[cfg(test)]
mod map_tests {
    use super::*;

    #[test]
    fn world_data_projects_and_colocated_photos_cluster() {
        assert_eq!(project(0.0, 0.0), World { x: 0.5, y: 0.5 });
        assert_eq!(land().len(), LAND.lines().count());
        assert_eq!(borders().len(), BORDERS.lines().count());
        let bubbles = cluster(
            &[(20.0, 20.0), (20.0, 20.0), (100.0, 100.0), (500.0, 500.0)],
            &[1, 1, 1, 1],
            200.0,
            200.0,
        );
        assert_eq!(bubbles.len(), 2);
        assert_eq!(bubbles[0].members, [0, 1]);
        assert_eq!(bubbles[0].photos, 2);
    }

    #[test]
    fn reads_gps_and_uses_hemisphere_signs() {
        let mut bytes = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
        fn entry(bytes: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: [u8; 4]) {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(&count.to_le_bytes());
            bytes.extend_from_slice(&value);
        }
        bytes.extend_from_slice(&1u16.to_le_bytes());
        entry(&mut bytes, 0x8825, 4, 1, 26u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        entry(&mut bytes, 1, 2, 2, [b'S', 0, 0, 0]);
        entry(&mut bytes, 2, 5, 3, 80u32.to_le_bytes());
        entry(&mut bytes, 3, 2, 2, [b'W', 0, 0, 0]);
        entry(&mut bytes, 4, 5, 3, 104u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for numerator in [33u32, 30, 0, 70, 45, 0] {
            bytes.extend_from_slice(&numerator.to_le_bytes());
            bytes.extend_from_slice(&1u32.to_le_bytes());
        }
        assert_eq!(
            embedded_coordinates(&bytes),
            Coordinates::new(-33.5, -70.75)
        );
        bytes[36] = b'?';
        assert_eq!(embedded_coordinates(&bytes), None);
    }

    #[tokio::test]
    async fn revised_or_hidden_photos_do_not_reuse_visible_locations() {
        use crate::library::AlbumSettings;
        use crate::model::{
            AccountId, MediaKind, NodeId, RevisionId, SourceId, SourceSpec, VolumeId,
        };

        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(dir.path().join("catalog.db")).await.unwrap();
        let account = AccountId::new("test-account").unwrap();
        let volume = VolumeId::new("test-volume").unwrap();
        let album = SourceSpec {
            id: SourceId::new("album").unwrap(),
            account: account.clone(),
            volume: volume.clone(),
            root: NodeId::new("album").unwrap(),
            label: "Photo album".into(),
        };
        let key = AssetKey {
            account,
            volume,
            node: NodeId::new("photo").unwrap(),
        };
        catalog
            .sync_photo_album(
                album.clone(),
                vec![crate::model::Asset {
                    key: key.clone(),
                    name: "photo.jpg".into(),
                    kind: MediaKind::Image,
                    mime: "image/jpeg".into(),
                    revision: Some(RevisionId::new("one").unwrap()),
                    size_bytes: Some(64),
                    sort_time: 1,
                    favorite: false,
                }],
            )
            .await
            .unwrap();
        let coordinates = Coordinates::new(48.86, 2.35);
        catalog
            .store_photo_locations(vec![(key.clone(), Some("one".into()), coordinates)])
            .await
            .unwrap();
        assert_eq!(
            catalog
                .photo_locations(vec![
                    (key.clone(), Some("one".into())),
                    (key.clone(), Some("two".into()))
                ])
                .await
                .unwrap(),
            vec![Some(coordinates), None]
        );
        catalog
            .set_album_settings(
                album.id,
                AlbumSettings {
                    hidden: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            catalog
                .visible_location_keys(vec![key])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
