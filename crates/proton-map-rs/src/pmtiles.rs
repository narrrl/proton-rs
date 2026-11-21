//! PMTiles v3: one file holding a whole tile pyramid, addressed by a Hilbert
//! curve tile id through a root directory and optional leaf directories.
//!
//! The format is specified at
//! <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>. This
//! reader keeps the whole archive in memory, so a lookup is a binary search
//! and a slice, never I/O.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use crate::{Error, TileId};

/// Length of the fixed header at the start of every archive.
pub const HEADER_LEN: usize = 127;

/// How a part of the archive is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    Unknown,
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl Compression {
    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => Self::None,
            2 => Self::Gzip,
            3 => Self::Brotli,
            4 => Self::Zstd,
            _ => Self::Unknown,
        }
    }

    /// The header byte for this compression.
    pub fn to_byte(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::None => 1,
            Self::Gzip => 2,
            Self::Brotli => 3,
            Self::Zstd => 4,
        }
    }
}

/// The fixed header of an archive.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub data_length: u64,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    /// 1 is Mapbox Vector Tile.
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// West, south, east, north in degrees.
    pub bounds: [f64; 4],
    pub center_zoom: u8,
    /// Longitude, latitude in degrees.
    pub center: [f64; 2],
}

impl Header {
    /// Parse the first [`HEADER_LEN`] bytes of an archive.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER_LEN || &bytes[..7] != b"PMTiles" {
            return Err(Error::Format("not a PMTiles archive"));
        }
        if bytes[7] != 3 {
            return Err(Error::Format("only PMTiles version 3 is supported"));
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let deg_at =
            |at: usize| f64::from(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())) / 1e7;
        Ok(Self {
            root_offset: u64_at(8),
            root_length: u64_at(16),
            metadata_offset: u64_at(24),
            metadata_length: u64_at(32),
            leaf_offset: u64_at(40),
            leaf_length: u64_at(48),
            data_offset: u64_at(56),
            data_length: u64_at(64),
            addressed_tiles: u64_at(72),
            tile_entries: u64_at(80),
            tile_contents: u64_at(88),
            clustered: bytes[96] == 1,
            internal_compression: Compression::from_byte(bytes[97]),
            tile_compression: Compression::from_byte(bytes[98]),
            tile_type: bytes[99],
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: [deg_at(102), deg_at(106), deg_at(110), deg_at(114)],
            center_zoom: bytes[118],
            center: [deg_at(119), deg_at(123)],
        })
    }

    /// The header as its [`HEADER_LEN`] bytes.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..7].copy_from_slice(b"PMTiles");
        out[7] = 3;
        let fields = [
            self.root_offset,
            self.root_length,
            self.metadata_offset,
            self.metadata_length,
            self.leaf_offset,
            self.leaf_length,
            self.data_offset,
            self.data_length,
            self.addressed_tiles,
            self.tile_entries,
            self.tile_contents,
        ];
        for (i, value) in fields.iter().enumerate() {
            out[8 + i * 8..16 + i * 8].copy_from_slice(&value.to_le_bytes());
        }
        out[96] = u8::from(self.clustered);
        out[97] = self.internal_compression.to_byte();
        out[98] = self.tile_compression.to_byte();
        out[99] = self.tile_type;
        out[100] = self.min_zoom;
        out[101] = self.max_zoom;
        let e7 = |deg: f64| ((deg * 1e7).round() as i32).to_le_bytes();
        for (i, deg) in self.bounds.iter().enumerate() {
            out[102 + i * 4..106 + i * 4].copy_from_slice(&e7(*deg));
        }
        out[118] = self.center_zoom;
        out[119..123].copy_from_slice(&e7(self.center[0]));
        out[123..127].copy_from_slice(&e7(self.center[1]));
        out
    }
}

/// One directory entry: a run of tiles sharing one content, or (with a run
/// length of 0) a pointer to a leaf directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// Append `value` as a protobuf-style varint.
pub fn write_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Read a varint at `*at`, advancing it.
pub fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, Error> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*at)
            .ok_or(Error::Format("varint runs past the end"))?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Format("varint longer than 64 bits"))
}

fn rotate(n: u64, x: &mut u64, y: &mut u64, rx: u64, ry: u64) {
    if ry == 0 {
        if rx == 1 {
            *x = n - 1 - *x;
            *y = n - 1 - *y;
        }
        std::mem::swap(x, y);
    }
}

/// The Hilbert tile id of `tile`: all tiles of lower zooms first, then the
/// tile's position on the curve through its zoom.
pub fn tile_id(tile: TileId) -> u64 {
    let TileId { z, x, y } = tile;
    let base = ((1u64 << (2 * u32::from(z))) - 1) / 3;
    let n = 1u64 << z;
    let (mut x, mut y) = (u64::from(x), u64::from(y));
    let mut d = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = u64::from(x & s > 0);
        let ry = u64::from(y & s > 0);
        d += s * s * ((3 * rx) ^ ry);
        rotate(n, &mut x, &mut y, rx, ry);
        s /= 2;
    }
    base + d
}

/// The tile a Hilbert tile id stands for; the inverse of [`tile_id`].
pub fn tile_from_id(id: u64) -> TileId {
    let mut z = 0u8;
    let mut base = 0u64;
    loop {
        let count = 1u64 << (2 * u32::from(z));
        if id < base + count {
            break;
        }
        base += count;
        z += 1;
    }
    let n = 1u64 << z;
    let mut t = id - base;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        rotate(s, &mut x, &mut y, rx, ry);
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    TileId::new(z, x as u32, y as u32)
}

/// Decode a directory (already decompressed).
pub fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>, Error> {
    let mut at = 0;
    let count = usize::try_from(read_varint(bytes, &mut at)?)
        .map_err(|_| Error::Format("directory too long"))?;
    if count > bytes.len() {
        return Err(Error::Format("directory entry count exceeds its size"));
    }
    let mut entries = vec![
        Entry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 0,
        };
        count
    ];
    let mut last = 0u64;
    for entry in &mut entries {
        last = last
            .checked_add(read_varint(bytes, &mut at)?)
            .ok_or(Error::Format("tile id overflow"))?;
        entry.tile_id = last;
    }
    for entry in &mut entries {
        entry.run_length = read_varint(bytes, &mut at)? as u32;
    }
    for entry in &mut entries {
        entry.length = read_varint(bytes, &mut at)? as u32;
    }
    for i in 0..count {
        let raw = read_varint(bytes, &mut at)?;
        entries[i].offset = if raw == 0 && i > 0 {
            entries[i - 1].offset + u64::from(entries[i - 1].length)
        } else {
            raw.saturating_sub(1)
        };
    }
    Ok(entries)
}

/// Encode a directory (before compression).
pub fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(entries.len() as u64, &mut out);
    let mut last = 0;
    for entry in entries {
        write_varint(entry.tile_id - last, &mut out);
        last = entry.tile_id;
    }
    for entry in entries {
        write_varint(u64::from(entry.run_length), &mut out);
    }
    for entry in entries {
        write_varint(u64::from(entry.length), &mut out);
    }
    for (i, entry) in entries.iter().enumerate() {
        let follows =
            i > 0 && entry.offset == entries[i - 1].offset + u64::from(entries[i - 1].length);
        write_varint(if follows { 0 } else { entry.offset + 1 }, &mut out);
    }
    out
}

/// The entry covering `id` in a sorted directory, if any: a tile run that
/// holds it, or the leaf directory that may.
fn find_entry(entries: &[Entry], id: u64) -> Option<Entry> {
    let index = entries.partition_point(|entry| entry.tile_id <= id);
    let entry = *entries.get(index.checked_sub(1)?)?;
    if entry.run_length == 0 || id < entry.tile_id + u64::from(entry.run_length) {
        Some(entry)
    } else {
        None
    }
}

pub(crate) fn decompress(bytes: &[u8], compression: Compression) -> Result<Vec<u8>, Error> {
    match compression {
        Compression::None | Compression::Unknown => Ok(bytes.to_vec()),
        Compression::Gzip => {
            let mut out = Vec::with_capacity(bytes.len() * 4);
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut out)
                .map_err(|_| Error::Format("broken gzip stream"))?;
            Ok(out)
        }
        Compression::Brotli | Compression::Zstd => Err(Error::Format(
            "only gzip or uncompressed archives are supported",
        )),
    }
}

/// An archive held in memory.
pub struct PmTiles {
    data: Arc<[u8]>,
    header: Header,
    root: Vec<Entry>,
    /// Leaf directories already parsed, by offset.
    leaves: Mutex<HashMap<u64, Arc<Vec<Entry>>>>,
}

impl std::fmt::Debug for PmTiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PmTiles")
            .field("bytes", &self.data.len())
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

impl PmTiles {
    /// Open an archive from its bytes.
    pub fn new(data: impl Into<Arc<[u8]>>) -> Result<Self, Error> {
        let data = data.into();
        let header = Header::parse(&data)?;
        let root_bytes = slice(&data, header.root_offset, header.root_length)?;
        let root = parse_directory(&decompress(root_bytes, header.internal_compression)?)?;
        Ok(Self {
            data,
            header,
            root,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The metadata JSON, decompressed.
    pub fn metadata(&self) -> Result<String, Error> {
        let bytes = slice(
            &self.data,
            self.header.metadata_offset,
            self.header.metadata_length,
        )?;
        let raw = decompress(bytes, self.header.internal_compression)?;
        String::from_utf8(raw).map_err(|_| Error::Format("metadata is not UTF-8"))
    }

    fn leaf(&self, offset: u64, length: u32) -> Result<Arc<Vec<Entry>>, Error> {
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(leaf) = leaves.get(&offset) {
            return Ok(leaf.clone());
        }
        let bytes = slice(
            &self.data,
            self.header.leaf_offset + offset,
            u64::from(length),
        )?;
        let leaf = Arc::new(parse_directory(&decompress(
            bytes,
            self.header.internal_compression,
        )?)?);
        leaves.insert(offset, leaf.clone());
        Ok(leaf)
    }

    /// The raw (still compressed) bytes of `tile`, or `None` if the archive
    /// doesn't hold it.
    pub fn tile_raw(&self, tile: TileId) -> Result<Option<&[u8]>, Error> {
        let id = tile_id(tile);
        let mut entry = match find_entry(&self.root, id) {
            Some(entry) => entry,
            None => return Ok(None),
        };
        // The spec allows at most three levels of leaves below the root.
        for _ in 0..4 {
            if entry.run_length > 0 {
                let bytes = slice(
                    &self.data,
                    self.header.data_offset + entry.offset,
                    u64::from(entry.length),
                )?;
                return Ok(Some(bytes));
            }
            let leaf = self.leaf(entry.offset, entry.length)?;
            entry = match find_entry(&leaf, id) {
                Some(entry) => entry,
                None => return Ok(None),
            };
        }
        Err(Error::Format("leaf directories nested too deep"))
    }

    /// The decompressed bytes of `tile`.
    pub fn tile(&self, tile: TileId) -> Result<Option<Vec<u8>>, Error> {
        match self.tile_raw(tile)? {
            Some(bytes) => Ok(Some(decompress(bytes, self.header.tile_compression)?)),
            None => Ok(None),
        }
    }
}

fn slice(data: &[u8], offset: u64, length: u64) -> Result<&[u8], Error> {
    let start = usize::try_from(offset).map_err(|_| Error::Format("offset out of range"))?;
    let len = usize::try_from(length).map_err(|_| Error::Format("length out of range"))?;
    data.get(start..start.saturating_add(len))
        .ok_or(Error::Format("section runs past the end of the archive"))
}

/// Writing archives, for the basemap builder and test fixtures.
#[cfg(feature = "writer")]
pub mod writer {
    use std::collections::HashMap;
    use std::io::Write;

    use super::{Compression, Entry, HEADER_LEN, Header, serialize_directory, tile_id};
    use crate::TileId;

    /// The root directory must fit, with the header, in the first 16 KiB.
    const ROOT_BUDGET: usize = 16_384 - HEADER_LEN;

    /// Builds an archive in memory. Tiles may be added in any order;
    /// identical contents are stored once and consecutive tile ids with the
    /// same content collapse into one run.
    #[derive(Debug, Default)]
    pub struct Writer {
        tiles: Vec<(u64, Vec<u8>)>,
        min_zoom: u8,
        max_zoom: u8,
        seen_zoom: bool,
    }

    /// Gzip `bytes` at the best compression.
    pub fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).expect("writing to a Vec");
        encoder.finish().expect("writing to a Vec")
    }

    impl Writer {
        pub fn new() -> Self {
            Self::default()
        }

        /// Add a tile; `bytes` are stored as given (already gzipped for a
        /// gzip archive).
        pub fn add(&mut self, tile: TileId, bytes: Vec<u8>) {
            if self.seen_zoom {
                self.min_zoom = self.min_zoom.min(tile.z);
                self.max_zoom = self.max_zoom.max(tile.z);
            } else {
                (self.min_zoom, self.max_zoom, self.seen_zoom) = (tile.z, tile.z, true);
            }
            self.tiles.push((tile_id(tile), bytes));
        }

        pub fn len(&self) -> usize {
            self.tiles.len()
        }

        pub fn is_empty(&self) -> bool {
            self.tiles.is_empty()
        }

        /// The finished archive. `metadata` is the JSON metadata; tiles are
        /// declared as gzipped MVT.
        pub fn finish(mut self, metadata: &str, bounds: [f64; 4], center: [f64; 2]) -> Vec<u8> {
            self.tiles.sort_by_key(|(id, _)| *id);
            self.tiles.dedup_by_key(|(id, _)| *id);

            let mut data = Vec::new();
            let mut offsets: HashMap<&[u8], u64> = HashMap::new();
            let mut entries: Vec<Entry> = Vec::new();
            for (id, bytes) in &self.tiles {
                let offset = match offsets.get(bytes.as_slice()) {
                    Some(&offset) => offset,
                    None => {
                        let offset = data.len() as u64;
                        data.extend_from_slice(bytes);
                        offsets.insert(bytes.as_slice(), offset);
                        offset
                    }
                };
                if let Some(last) = entries.last_mut()
                    && last.offset == offset
                    && last.tile_id + u64::from(last.run_length) == *id
                {
                    last.run_length += 1;
                    continue;
                }
                entries.push(Entry {
                    tile_id: *id,
                    offset,
                    length: bytes.len() as u32,
                    run_length: 1,
                });
            }
            let contents = offsets.len() as u64;

            let (root, leaves) = build_directories(&entries);
            let metadata = gzip(metadata.as_bytes());

            let root_offset = HEADER_LEN as u64;
            let metadata_offset = root_offset + root.len() as u64;
            let leaf_offset = metadata_offset + metadata.len() as u64;
            let data_offset = leaf_offset + leaves.len() as u64;
            let header = Header {
                root_offset,
                root_length: root.len() as u64,
                metadata_offset,
                metadata_length: metadata.len() as u64,
                leaf_offset,
                leaf_length: leaves.len() as u64,
                data_offset,
                data_length: data.len() as u64,
                addressed_tiles: self.tiles.len() as u64,
                tile_entries: entries.len() as u64,
                tile_contents: contents,
                clustered: true,
                internal_compression: Compression::Gzip,
                tile_compression: Compression::Gzip,
                tile_type: 1,
                min_zoom: self.min_zoom,
                max_zoom: self.max_zoom,
                bounds,
                center_zoom: 0,
                center,
            };
            let mut out = Vec::with_capacity(data_offset as usize + data.len());
            out.extend_from_slice(&header.to_bytes());
            out.extend_from_slice(&root);
            out.extend_from_slice(&metadata);
            out.extend_from_slice(&leaves);
            out.extend_from_slice(&data);
            out
        }
    }

    /// The compressed root directory and the leaf section. With too many
    /// entries for the root, entries go into leaves of a growing size until
    /// the root of pointers to them fits.
    fn build_directories(entries: &[Entry]) -> (Vec<u8>, Vec<u8>) {
        let root = gzip(&serialize_directory(entries));
        if root.len() <= ROOT_BUDGET {
            return (root, Vec::new());
        }
        let mut leaf_size = 4096usize;
        loop {
            let mut leaves = Vec::new();
            let mut pointers = Vec::new();
            for chunk in entries.chunks(leaf_size) {
                let leaf = gzip(&serialize_directory(chunk));
                pointers.push(Entry {
                    tile_id: chunk[0].tile_id,
                    offset: leaves.len() as u64,
                    length: leaf.len() as u32,
                    run_length: 0,
                });
                leaves.extend_from_slice(&leaf);
            }
            let root = gzip(&serialize_directory(&pointers));
            if root.len() <= ROOT_BUDGET {
                return (root, leaves);
            }
            leaf_size *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for value in [
            0,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut bytes = Vec::new();
            write_varint(value, &mut bytes);
            let mut at = 0;
            assert_eq!(read_varint(&bytes, &mut at).unwrap(), value);
            assert_eq!(at, bytes.len());
        }
        assert!(read_varint(&[0x80], &mut 0).is_err());
    }

    #[test]
    fn hilbert_ids_match_the_spec_and_round_trip() {
        assert_eq!(tile_id(TileId::new(0, 0, 0)), 0);
        assert_eq!(tile_id(TileId::new(1, 0, 0)), 1);
        assert_eq!(tile_id(TileId::new(1, 0, 1)), 2);
        assert_eq!(tile_id(TileId::new(1, 1, 1)), 3);
        assert_eq!(tile_id(TileId::new(1, 1, 0)), 4);
        assert_eq!(tile_id(TileId::new(2, 0, 0)), 5);
        for z in 0..=6u8 {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    let tile = TileId::new(z, x, y);
                    assert_eq!(tile_from_id(tile_id(tile)), tile);
                }
            }
        }
        let deep = TileId::new(20, 543_210, 345_678);
        assert_eq!(tile_from_id(tile_id(deep)), deep);
    }

    #[test]
    fn directories_round_trip() {
        let entries = vec![
            Entry {
                tile_id: 3,
                offset: 0,
                length: 10,
                run_length: 1,
            },
            Entry {
                tile_id: 4,
                offset: 10,
                length: 5,
                run_length: 7,
            },
            Entry {
                tile_id: 40,
                offset: 0,
                length: 10,
                run_length: 1,
            },
        ];
        assert_eq!(
            parse_directory(&serialize_directory(&entries)).unwrap(),
            entries
        );
    }

    #[cfg(feature = "writer")]
    #[test]
    fn written_archives_read_back_with_runs_and_leaves() {
        use super::writer::Writer;

        let mut writer = Writer::new();
        // Enough distinct tiles to need leaf directories.
        let z = 9u8;
        for x in 0..128u32 {
            for y in 0..128u32 {
                let bytes = if (x + y) % 3 == 0 {
                    b"ocean".to_vec()
                } else {
                    format!("{x}/{y}").into_bytes()
                };
                writer.add(TileId::new(z, x, y), writer::gzip(&bytes));
            }
        }
        writer.add(TileId::new(0, 0, 0), writer::gzip(b"world"));
        let archive = writer.finish("{}", [-180.0, -85.0, 180.0, 85.0], [0.0, 0.0]);
        let pm = PmTiles::new(archive).unwrap();
        assert!(pm.header().leaf_length > 0, "expected leaf directories");
        assert_eq!(pm.header().min_zoom, 0);
        assert_eq!(pm.header().max_zoom, 9);
        assert_eq!(pm.metadata().unwrap(), "{}");
        assert_eq!(pm.tile(TileId::new(0, 0, 0)).unwrap().unwrap(), b"world");
        for (x, y) in [(0, 0), (1, 2), (127, 127), (57, 120), (3, 0)] {
            let want = if (x + y) % 3 == 0 {
                b"ocean".to_vec()
            } else {
                format!("{x}/{y}").into_bytes()
            };
            assert_eq!(pm.tile(TileId::new(z, x, y)).unwrap().unwrap(), want);
        }
        assert!(pm.tile(TileId::new(9, 300, 300)).unwrap().is_none());
        assert!(pm.tile(TileId::new(5, 1, 1)).unwrap().is_none());
        assert!(pm.header().tile_contents < pm.header().addressed_tiles);
    }
}
