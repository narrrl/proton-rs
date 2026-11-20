//! Which photos are one shot: a camera's RAW+JPEG pair, a phone's raw capture
//! and its cover, a live photo and its still.
//!
//! The grouping itself is pure. The caller supplies each photo's local capture
//! day (`YYYY-MM-DD`): grouping by day rather than by exact second is
//! deliberate — a camera writes the RAW and the JPEG of one shot a moment apart,
//! and two shots of the same subject on different days are not one photo. A
//! SQLite catalog gets it from
//! `strftime('%Y-%m-%d', capture_time, 'unixepoch', 'localtime')`.

use std::collections::HashMap;

/// What a file is, for grouping and for choosing how to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaClass {
    /// A normal, directly viewable still image (JPEG, PNG, HEIC, …).
    Photo,
    /// A video clip (mp4, mkv, mov, …).
    Video,
    /// A camera raw file (CR2, NEF, DNG, …).
    Raw,
}

/// Video file extensions, lowercase.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "avi", "webm", "m4v", "flv", "wmv", "mpg", "mpeg", "ts", "3gp", "m2ts",
    "mts", "ogv",
];

/// Camera raw file extensions, lowercase.
pub const RAW_EXTENSIONS: &[&str] = &[
    "cr2", "cr3", "nef", "nrw", "arw", "srf", "sr2", "dng", "raf", "orf", "rw2", "srw", "pef",
    "raw", "rwl", "iiq", "3fr", "dcr", "kdc", "mrw", "x3f",
];

/// Still image file extensions, lowercase.
pub const PHOTO_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "tif", "tiff",
];

impl MediaClass {
    /// Classify a file. A recognised extension is authoritative — it is there
    /// before the media type resolves, and a raw file's media type is often a
    /// useless generic one — so the media type is only consulted when the name
    /// carries no extension we know. Anything still unresolved is a photo.
    pub fn classify(name: Option<&str>, media_type: Option<&str>) -> MediaClass {
        if let Some(ext) = name
            .and_then(|n| n.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
        {
            if RAW_EXTENSIONS.contains(&ext.as_str()) {
                return MediaClass::Raw;
            }
            if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
                return MediaClass::Video;
            }
            if PHOTO_EXTENSIONS.contains(&ext.as_str()) {
                return MediaClass::Photo;
            }
        }
        if let Some(mt) = media_type
            && mt.starts_with("video/")
        {
            return MediaClass::Video;
        }
        MediaClass::Photo
    }
}

/// The file name without its extension, lowercased — `IMG_1234.CR2` and
/// `IMG_1234.JPG` share `img_1234` — with the camera's own part-of-a-shot
/// marker removed, so the two files a raw capture writes share a stem too.
pub fn name_stem(name: Option<&str>) -> Option<String> {
    let name = name?;
    let stem = match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    };
    let stem = stem.to_ascii_lowercase();
    Some(strip_shot_marker(&stem).to_string())
}

/// Drop a `.RAW-NN.ROLE` marker from a stem.
///
/// Pixel stores one raw capture as `PXL_20260919_000625670.RAW-01.COVER.jpg`
/// and `PXL_20260919_000625670.RAW-02.ORIGINAL.dng`: one shot, two files, two
/// stems that differ in exactly the part naming which file of the shot this is.
/// Cutting the marker off is what lets rule 2 see the pair. The check is narrow
/// on purpose — digits, then a single role word — so an ordinary name that
/// happens to contain `.raw-` keeps its stem.
pub fn strip_shot_marker(stem: &str) -> &str {
    let Some(at) = stem.rfind(".raw-") else {
        return stem;
    };
    let rest = &stem[at + ".raw-".len()..];
    let Some((index, role)) = rest.split_once('.') else {
        return stem;
    };
    let marked = !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
        && !role.is_empty()
        && role.bytes().all(|b| b.is_ascii_alphabetic());
    if marked { &stem[..at] } else { stem }
}

/// One entry of the grouping pass: what rule 2 needs, plus what choosing a
/// representative needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grouped {
    /// The photo's identity; group keys are drawn from these.
    pub uid: String,
    /// Whether the file is a camera raw ([`MediaClass::Raw`]).
    pub raw: bool,
    /// The server's own relation: the uid of the photo this one belongs to.
    pub main_uid: Option<String>,
    /// The local capture day, `YYYY-MM-DD`; see the module docs.
    pub day: Option<String>,
    /// [`name_stem`] of the file name.
    pub stem: Option<String>,
}

/// Which group each photo belongs to, and which member of it the gallery shows.
///
/// Precedence, as agreed:
///
/// 1. The server's own relation — a photo with a `main_photo_uid` joins that
///    photo's group. The server knows about live photos and bursts; we do not
///    have to guess at them.
/// 2. Otherwise, same capture day **and** same file-name stem **and** one of the
///    two is a camera raw while the other is not. That is the RAW+JPEG pair a
///    camera writes, and the differing-kind clause keeps two JPEGs that happen
///    to share a name apart.
/// 3. Otherwise the photo is its own group.
///
/// The representative is the non-raw member when the group has one — a JPEG
/// decodes in milliseconds and is what the person expects to see — and the
/// earliest member in server order otherwise.
pub fn group_photos(entries: &[Grouped]) -> Vec<String> {
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let union = |parent: &mut [usize], a: usize, b: usize| {
        let (a, b) = (find(parent, a), find(parent, b));
        if a != b {
            parent[b] = a;
        }
    };

    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.uid.as_str(), i))
        .collect();
    for (i, entry) in entries.iter().enumerate() {
        if let Some(main) = entry.main_uid.as_deref()
            && let Some(&j) = index.get(main)
        {
            union(&mut parent, j, i);
        }
    }

    // Rule 2 needs only one pass: everything sharing a day and a stem lands in
    // the same bucket, and the raw / non-raw split is checked pairwise there.
    let mut by_name: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        if let (Some(day), Some(stem)) = (entry.day.as_deref(), entry.stem.as_deref()) {
            by_name.entry((day, stem)).or_default().push(i);
        }
    }
    let is_raw = |i: usize| entries[i].raw;
    for bucket in by_name.values() {
        for (n, &i) in bucket.iter().enumerate() {
            for &j in &bucket[n + 1..] {
                if is_raw(i) != is_raw(j) {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    // Representative per root, then the key every member carries.
    let mut representative: HashMap<usize, usize> = HashMap::new();
    for i in 0..entries.len() {
        let root = find(&mut parent, i);
        match representative.get(&root).copied() {
            Some(current) if !is_raw(current) || is_raw(i) => {}
            _ => {
                representative.insert(root, i);
            }
        }
    }
    (0..entries.len())
        .map(|i| {
            let root = find(&mut parent, i);
            entries[representative[&root]].uid.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(uid: &str, name: &str, day: &str, main: Option<&str>) -> Grouped {
        Grouped {
            uid: uid.into(),
            raw: MediaClass::classify(Some(name), None) == MediaClass::Raw,
            main_uid: main.map(Into::into),
            day: Some(day.into()),
            stem: name_stem(Some(name)),
        }
    }

    #[test]
    fn a_raw_and_its_jpeg_group_under_the_jpeg() {
        let groups = group_photos(&[
            photo("raw", "IMG_1234.CR2", "2026-01-01", None),
            photo("jpg", "IMG_1234.JPG", "2026-01-01", None),
        ]);
        assert_eq!(groups, ["jpg", "jpg"]);
    }

    #[test]
    fn a_pixel_raw_capture_groups_with_its_cover() {
        let groups = group_photos(&[
            photo(
                "dng",
                "PXL_20260919_000625670.RAW-02.ORIGINAL.dng",
                "2026-09-19",
                None,
            ),
            photo(
                "cover",
                "PXL_20260919_000625670.RAW-01.COVER.jpg",
                "2026-09-19",
                None,
            ),
        ]);
        assert_eq!(groups, ["cover", "cover"]);
    }

    #[test]
    fn a_shared_name_alone_does_not_group() {
        let groups = group_photos(&[
            photo("a", "IMG_1.JPG", "2026-01-01", None),
            photo("b", "IMG_1.JPG", "2026-01-01", None),
            photo("c", "IMG_1.CR2", "2026-01-02", None),
        ]);
        assert_eq!(groups, ["a", "b", "c"]);
    }

    #[test]
    fn the_servers_relation_groups_a_live_photo() {
        let groups = group_photos(&[
            photo("still", "IMG_7.HEIC", "2026-01-01", None),
            photo("clip", "IMG_7_live.MOV", "2026-01-03", Some("still")),
        ]);
        assert_eq!(groups, ["still", "still"]);
    }

    #[test]
    fn an_ordinary_name_keeps_its_raw_looking_part() {
        assert_eq!(strip_shot_marker("trip.raw-notes"), "trip.raw-notes");
        assert_eq!(strip_shot_marker("pxl_1.raw-01.cover"), "pxl_1");
        assert_eq!(name_stem(Some("Holiday.JPG")).as_deref(), Some("holiday"));
    }

    #[test]
    fn classify_trusts_the_extension_over_the_media_type() {
        assert_eq!(
            MediaClass::classify(Some("a.DNG"), Some("image/jpeg")),
            MediaClass::Raw
        );
        assert_eq!(
            MediaClass::classify(Some("noext"), Some("video/mp4")),
            MediaClass::Video
        );
        assert_eq!(MediaClass::classify(None, None), MediaClass::Photo);
    }
}
