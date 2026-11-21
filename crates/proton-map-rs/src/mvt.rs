//! Mapbox Vector Tiles: protobuf, one message per tile, holding named layers
//! of point, line and polygon features with key/value tags.
//!
//! The decoder reads only what drawing needs — layer names, tags and
//! geometry. The format is at <https://github.com/mapbox/vector-tile-spec>.

use crate::Error;

/// One layer of a tile, such as `land` or `places`.
#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub name: String,
    /// The tile's width in geometry units; 4096 almost everywhere.
    pub extent: u32,
    pub keys: Vec<String>,
    pub values: Vec<Value>,
    pub features: Vec<Feature>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Text(String),
    Number(f64),
    Bool(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Point,
    Line,
    Polygon,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub kind: Kind,
    /// Pairs of indices into the layer's keys and values.
    pub tags: Vec<(u32, u32)>,
    /// Points, lines or polygon rings, in tile units. A point feature holds
    /// one part with all its points. Rings are not repeated at their end.
    pub parts: Vec<Vec<(f32, f32)>>,
}

impl Layer {
    /// The value `feature` carries for `key`.
    pub fn tag(&self, feature: &Feature, key: &str) -> Option<&Value> {
        feature.tags.iter().find_map(|&(k, v)| {
            (self.keys.get(k as usize)? == key).then(|| self.values.get(v as usize))?
        })
    }

    pub fn text(&self, feature: &Feature, key: &str) -> Option<&str> {
        match self.tag(feature, key)? {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn number(&self, feature: &Feature, key: &str) -> Option<f64> {
        match self.tag(feature, key)? {
            Value::Number(n) => Some(*n),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Text(_) => None,
        }
    }
}

/// Every layer of the tile in `data` (decompressed).
pub fn decode(data: &[u8]) -> Result<Vec<Layer>, Error> {
    let mut tile = Reader::new(data);
    let mut layers = Vec::new();
    while let Some((field, wire)) = tile.field()? {
        match field {
            3 => layers.push(decode_layer(tile.bytes()?)?),
            _ => tile.skip(wire)?,
        }
    }
    Ok(layers)
}

fn decode_layer(data: &[u8]) -> Result<Layer, Error> {
    let mut layer = Layer {
        name: String::new(),
        extent: 4096,
        keys: Vec::new(),
        values: Vec::new(),
        features: Vec::new(),
    };
    let mut reader = Reader::new(data);
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => layer.name = reader.text()?,
            2 => {
                if let Some(feature) = decode_feature(reader.bytes()?)? {
                    layer.features.push(feature);
                }
            }
            3 => layer.keys.push(reader.text()?),
            4 => layer.values.push(decode_value(reader.bytes()?)?),
            5 => layer.extent = reader.varint()?.clamp(1, u64::from(u32::MAX)) as u32,
            _ => reader.skip(wire)?,
        }
    }
    Ok(layer)
}

fn decode_value(data: &[u8]) -> Result<Value, Error> {
    let mut reader = Reader::new(data);
    let mut value = Value::Bool(false);
    while let Some((field, wire)) = reader.field()? {
        value = match field {
            1 => Value::Text(reader.text()?),
            2 => Value::Number(f64::from(f32::from_bits(reader.fixed32()?))),
            3 => Value::Number(f64::from_bits(reader.fixed64()?)),
            4 | 5 => Value::Number(reader.varint()? as i64 as f64),
            6 => Value::Number(zigzag(reader.varint()?) as f64),
            7 => Value::Bool(reader.varint()? != 0),
            _ => {
                reader.skip(wire)?;
                continue;
            }
        };
    }
    Ok(value)
}

/// A feature, or `None` for one of an unknown geometry type.
fn decode_feature(data: &[u8]) -> Result<Option<Feature>, Error> {
    let mut reader = Reader::new(data);
    let mut kind = None;
    let mut tags = Vec::new();
    let mut geometry = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            2 => {
                let packed = reader.packed()?;
                tags = packed
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&[k, v]| (k as u32, v as u32))
                    .collect();
            }
            3 => {
                kind = match reader.varint()? {
                    1 => Some(Kind::Point),
                    2 => Some(Kind::Line),
                    3 => Some(Kind::Polygon),
                    _ => None,
                }
            }
            4 => geometry = reader.packed()?,
            _ => reader.skip(wire)?,
        }
    }
    let Some(kind) = kind else {
        return Ok(None);
    };
    Ok(Some(Feature {
        kind,
        tags,
        parts: decode_geometry(kind, &geometry)?,
    }))
}

/// Turn the command stream of a feature's geometry into its parts. Positions
/// are deltas from the previous one, carried across parts.
pub fn decode_geometry(kind: Kind, commands: &[u64]) -> Result<Vec<Vec<(f32, f32)>>, Error> {
    let mut parts: Vec<Vec<(f32, f32)>> = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    let mut i = 0;
    while i < commands.len() {
        let command = commands[i] & 7;
        let count = (commands[i] >> 3) as usize;
        i += 1;
        match command {
            // MoveTo starts a part — except for points, which all share one.
            1 | 2 => {
                if count > (commands.len() - i) / 2 {
                    return Err(Error::Format("geometry command runs past the end"));
                }
                for n in 0..count {
                    x += zigzag(commands[i + 2 * n]);
                    y += zigzag(commands[i + 2 * n + 1]);
                    let starts = command == 1 && (kind != Kind::Point || parts.is_empty());
                    if starts {
                        parts.push(Vec::new());
                    }
                    parts
                        .last_mut()
                        .ok_or(Error::Format("LineTo before MoveTo"))?
                        .push((x as f32, y as f32));
                }
                i += 2 * count;
            }
            // ClosePath: the ring's end joins its start, which drawing does.
            7 => {}
            _ => return Err(Error::Format("unknown geometry command")),
        }
    }
    Ok(parts)
}

fn zigzag(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// The protobuf wire format, as far as vector tiles use it.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

const TRUNCATED: Error = Error::Format("truncated vector tile");

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    /// The next field's number and wire type; `None` at the end.
    fn field(&mut self) -> Result<Option<(u64, u8)>, Error> {
        if self.at >= self.data.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        Ok(Some((key >> 3, (key & 7) as u8)))
    }

    fn varint(&mut self) -> Result<u64, Error> {
        crate::pmtiles::read_varint(self.data, &mut self.at).map_err(|_| TRUNCATED)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).ok_or(TRUNCATED)?;
        let bytes = self.data.get(self.at..end).ok_or(TRUNCATED)?;
        self.at = end;
        Ok(bytes)
    }

    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = usize::try_from(self.varint()?).map_err(|_| TRUNCATED)?;
        self.take(len)
    }

    fn text(&mut self) -> Result<String, Error> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    fn fixed32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().map_err(|_| TRUNCATED)?,
        ))
    }

    fn fixed64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().map_err(|_| TRUNCATED)?,
        ))
    }

    fn packed(&mut self) -> Result<Vec<u64>, Error> {
        let mut inner = Reader::new(self.bytes()?);
        let mut values = Vec::new();
        while inner.at < inner.data.len() {
            values.push(inner.varint()?);
        }
        Ok(values)
    }

    fn skip(&mut self, wire: u8) -> Result<(), Error> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.take(4)?;
            }
            _ => return Err(Error::Format("unknown protobuf wire type")),
        }
        Ok(())
    }
}

/// Encoding tiles, for the basemap builder and test fixtures.
#[cfg(feature = "writer")]
pub mod encode {
    use std::collections::HashMap;

    use super::Kind;
    use crate::pmtiles::write_varint;

    /// A tag value to encode.
    #[derive(Debug, Clone, PartialEq)]
    pub enum Tag {
        Text(String),
        Int(i64),
        Double(f64),
        Bool(bool),
    }

    impl Tag {
        /// A key for de-duplicating values within a layer.
        fn key(&self) -> (u8, String) {
            match self {
                Tag::Text(s) => (1, s.clone()),
                Tag::Int(n) => (2, n.to_string()),
                Tag::Double(d) => (3, d.to_bits().to_string()),
                Tag::Bool(b) => (4, b.to_string()),
            }
        }
    }

    /// Builds one layer. Coordinates are integer tile units.
    #[derive(Debug)]
    pub struct LayerBuilder {
        name: String,
        extent: u32,
        keys: Vec<String>,
        key_index: HashMap<String, u32>,
        values: Vec<Tag>,
        value_index: HashMap<(u8, String), u32>,
        features: Vec<Vec<u8>>,
    }

    fn field_bytes(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        write_varint(number << 3 | 2, out);
        write_varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn zz(n: i64) -> u64 {
        ((n << 1) ^ (n >> 63)) as u64
    }

    impl LayerBuilder {
        pub fn new(name: &str, extent: u32) -> Self {
            Self {
                name: name.to_owned(),
                extent,
                keys: Vec::new(),
                key_index: HashMap::new(),
                values: Vec::new(),
                value_index: HashMap::new(),
                features: Vec::new(),
            }
        }

        pub fn is_empty(&self) -> bool {
            self.features.is_empty()
        }

        /// Add a feature. Polygon rings must not repeat their first point;
        /// parts with too few points for their kind are skipped.
        pub fn add(&mut self, kind: Kind, parts: &[Vec<(i32, i32)>], tags: &[(&str, Tag)]) {
            let min = match kind {
                Kind::Point => 1,
                Kind::Line => 2,
                Kind::Polygon => 3,
            };
            let mut commands: Vec<u64> = Vec::new();
            let (mut cx, mut cy) = (0i64, 0i64);
            let mut points_header = None;
            for part in parts.iter().filter(|part| part.len() >= min) {
                match kind {
                    Kind::Point => {
                        if points_header.is_none() {
                            points_header = Some(commands.len());
                            commands.push(0);
                        }
                        for &(x, y) in part {
                            commands.push(zz(i64::from(x) - cx));
                            commands.push(zz(i64::from(y) - cy));
                            (cx, cy) = (i64::from(x), i64::from(y));
                        }
                    }
                    Kind::Line | Kind::Polygon => {
                        let (x, y) = part[0];
                        commands.push(1 | 1 << 3);
                        commands.push(zz(i64::from(x) - cx));
                        commands.push(zz(i64::from(y) - cy));
                        (cx, cy) = (i64::from(x), i64::from(y));
                        commands.push(2 | ((part.len() as u64 - 1) << 3));
                        for &(x, y) in &part[1..] {
                            commands.push(zz(i64::from(x) - cx));
                            commands.push(zz(i64::from(y) - cy));
                            (cx, cy) = (i64::from(x), i64::from(y));
                        }
                        if kind == Kind::Polygon {
                            commands.push(7 | 1 << 3);
                        }
                    }
                }
            }
            if let Some(at) = points_header {
                let count = (commands.len() - at - 1) / 2;
                commands[at] = 1 | (count as u64) << 3;
            }
            if commands.is_empty() {
                return;
            }

            let mut tag_ids = Vec::new();
            for (key, value) in tags {
                let k = match self.key_index.get(*key) {
                    Some(&k) => k,
                    None => {
                        let k = self.keys.len() as u32;
                        self.keys.push((*key).to_owned());
                        self.key_index.insert((*key).to_owned(), k);
                        k
                    }
                };
                let v = match self.value_index.get(&value.key()) {
                    Some(&v) => v,
                    None => {
                        let v = self.values.len() as u32;
                        self.values.push(value.clone());
                        self.value_index.insert(value.key(), v);
                        v
                    }
                };
                tag_ids.push(u64::from(k));
                tag_ids.push(u64::from(v));
            }

            let mut feature = Vec::new();
            if !tag_ids.is_empty() {
                let mut packed = Vec::new();
                for t in tag_ids {
                    write_varint(t, &mut packed);
                }
                field_bytes(2, &packed, &mut feature);
            }
            write_varint(3 << 3, &mut feature);
            write_varint(
                match kind {
                    Kind::Point => 1,
                    Kind::Line => 2,
                    Kind::Polygon => 3,
                },
                &mut feature,
            );
            let mut packed = Vec::new();
            for c in commands {
                write_varint(c, &mut packed);
            }
            field_bytes(4, &packed, &mut feature);
            self.features.push(feature);
        }

        /// The encoded layer message.
        fn encode(&self) -> Vec<u8> {
            let mut out = Vec::new();
            write_varint(15 << 3, &mut out);
            write_varint(2, &mut out);
            field_bytes(1, self.name.as_bytes(), &mut out);
            for feature in &self.features {
                field_bytes(2, feature, &mut out);
            }
            for key in &self.keys {
                field_bytes(3, key.as_bytes(), &mut out);
            }
            for value in &self.values {
                let mut v = Vec::new();
                match value {
                    Tag::Text(s) => field_bytes(1, s.as_bytes(), &mut v),
                    Tag::Double(d) => {
                        write_varint(3 << 3 | 1, &mut v);
                        v.extend_from_slice(&d.to_bits().to_le_bytes());
                    }
                    Tag::Int(n) => {
                        write_varint(6 << 3, &mut v);
                        write_varint(zz(*n), &mut v);
                    }
                    Tag::Bool(b) => {
                        write_varint(7 << 3, &mut v);
                        write_varint(u64::from(*b), &mut v);
                    }
                }
                field_bytes(4, &v, &mut out);
            }
            write_varint(5 << 3, &mut out);
            write_varint(u64::from(self.extent), &mut out);
            out
        }
    }

    /// A tile of the given layers; empty layers are left out.
    pub fn tile(layers: &[LayerBuilder]) -> Vec<u8> {
        let mut out = Vec::new();
        for layer in layers.iter().filter(|layer| !layer.is_empty()) {
            field_bytes(3, &layer.encode(), &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(n: u64, out: &mut Vec<u8>) {
        crate::pmtiles::write_varint(n, out);
    }

    fn field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(number << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    fn packed(values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        for &v in values {
            varint(v, &mut out);
        }
        out
    }

    fn zz(n: i64) -> u64 {
        ((n << 1) ^ (n >> 63)) as u64
    }

    /// A hand-built tile with one `water` layer holding one square tagged
    /// class=lake.
    fn lake_tile() -> Vec<u8> {
        let mut feature = Vec::new();
        field(2, &packed(&[0, 0]), &mut feature);
        varint(3 << 3, &mut feature);
        varint(3, &mut feature);
        let square = [
            1 | 1 << 3,
            zz(10),
            zz(10),
            2 | 3 << 3,
            zz(20),
            zz(0),
            zz(0),
            zz(20),
            zz(-20),
            zz(0),
            7 | 1 << 3,
        ];
        field(4, &packed(&square), &mut feature);

        let mut value = Vec::new();
        field(1, b"lake", &mut value);

        let mut layer = Vec::new();
        field(1, b"water", &mut layer);
        field(2, &feature, &mut layer);
        field(3, b"class", &mut layer);
        field(4, &value, &mut layer);
        varint(5 << 3, &mut layer);
        varint(4096, &mut layer);

        let mut tile = Vec::new();
        field(3, &layer, &mut tile);
        tile
    }

    #[test]
    fn a_hand_built_polygon_decodes_with_its_tags() {
        let layers = decode(&lake_tile()).unwrap();
        let [water] = &layers[..] else {
            panic!("one layer expected");
        };
        assert_eq!(water.name, "water");
        assert_eq!(water.extent, 4096);
        let feature = &water.features[0];
        assert_eq!(feature.kind, Kind::Polygon);
        assert_eq!(water.text(feature, "class"), Some("lake"));
        assert_eq!(water.text(feature, "name"), None);
        assert_eq!(
            feature.parts,
            vec![vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)]]
        );
    }

    #[test]
    fn a_cut_off_tile_is_rejected() {
        let tile = lake_tile();
        assert!(decode(&tile[..tile.len() - 3]).is_err());
    }

    #[test]
    fn points_of_one_feature_share_a_part() {
        let commands = [1 | 2 << 3, zz(5), zz(5), zz(1), zz(-1)];
        let parts = decode_geometry(Kind::Point, &commands).unwrap();
        assert_eq!(parts, vec![vec![(5.0, 5.0), (6.0, 4.0)]]);
    }

    #[cfg(feature = "writer")]
    #[test]
    fn encoded_tiles_decode_to_the_same_features() {
        use encode::{LayerBuilder, Tag, tile};

        let mut places = LayerBuilder::new("places", 4096);
        places.add(
            Kind::Point,
            &[vec![(100, 200)]],
            &[
                ("name", Tag::Text("Köln".into())),
                ("pop", Tag::Int(1_000_000)),
            ],
        );
        let mut rivers = LayerBuilder::new("rivers", 4096);
        rivers.add(
            Kind::Line,
            &[vec![(-10, 5), (50, 60), (4200, 70)]],
            &[("rank", Tag::Int(-3))],
        );
        let mut land = LayerBuilder::new("land", 4096);
        land.add(
            Kind::Polygon,
            &[
                vec![(0, 0), (10, 0), (10, 10)],
                vec![(1, 1), (2, 1), (2, 2)],
            ],
            &[],
        );
        let layers = decode(&tile(&[places, rivers, land])).unwrap();
        assert_eq!(layers.len(), 3);
        let place = &layers[0].features[0];
        assert_eq!(layers[0].text(place, "name"), Some("Köln"));
        assert_eq!(layers[0].number(place, "pop"), Some(1_000_000.0));
        assert_eq!(place.parts, vec![vec![(100.0, 200.0)]]);
        let river = &layers[1].features[0];
        assert_eq!(layers[1].number(river, "rank"), Some(-3.0));
        assert_eq!(
            river.parts,
            vec![vec![(-10.0, 5.0), (50.0, 60.0), (4200.0, 70.0)]]
        );
        assert_eq!(layers[2].features[0].parts.len(), 2);
    }
}
