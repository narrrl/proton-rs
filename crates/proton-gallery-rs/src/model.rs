use serde::{Deserialize, Serialize};

use crate::{Error, Result};

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 1024 || value.contains('\0') {
                    return Err(Error::Invalid(
                        concat!(stringify!($name), " is invalid").into(),
                    ));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identifier!(AccountId);
identifier!(VolumeId);
identifier!(NodeId);
identifier!(SourceId);
identifier!(RevisionId);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssetKey {
    pub account: AccountId,
    pub volume: VolumeId,
    pub node: NodeId,
}

impl AssetKey {
    /// Filename-safe, unambiguous identity across accounts and volumes.
    pub fn stable_id(&self) -> Result<String> {
        use sha2::Digest;
        Ok(hex::encode(sha2::Sha256::digest(serde_json::to_vec(self)?)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub id: SourceId,
    pub account: AccountId,
    pub volume: VolumeId,
    pub root: NodeId,
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
}

impl MediaKind {
    /// MIME is authoritative; filename fallback handles generic upload types only.
    pub fn classify(mime: &str, name: &str) -> Option<Self> {
        let mime = mime
            .split(';')
            .next()
            .unwrap_or(mime)
            .trim()
            .to_ascii_lowercase();
        if mime.starts_with("image/") {
            return Some(Self::Image);
        }
        if mime.starts_with("video/") {
            return Some(Self::Video);
        }
        if !mime.is_empty() && mime != "application/octet-stream" {
            return None;
        }
        match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "heic" | "heif" | "avif" | "gif" | "tif" | "tiff"
            | "bmp" | "dng" => Some(Self::Image),
            "mp4" | "mov" | "mkv" | "webm" | "m4v" | "avi" | "mts" => Some(Self::Video),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub key: AssetKey,
    pub name: String,
    pub kind: MediaKind,
    pub mime: String,
    pub revision: Option<RevisionId>,
    /// Plaintext bytes only. Unknown must not be confused with encrypted size.
    pub size_bytes: Option<u64>,
    /// Current sorting fallback (remote mtime for Drive); not an EXIF claim.
    pub sort_time: i64,
    /// Marked as a favorite: the Proton `Favorite` photo tag, or the phone's own
    /// favorite flag. Left out of the stored record when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSize(usize);

impl PageSize {
    pub const MAX: usize = 250;

    pub fn new(value: usize) -> Result<Self> {
        if !(1..=Self::MAX).contains(&value) {
            return Err(Error::Invalid(format!(
                "page size must be 1..={}",
                Self::MAX
            )));
        }
        Ok(Self(value))
    }

    pub fn get(self) -> usize {
        self.0
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self(100)
    }
}

/// Order of a media grid. Name order ignores case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Newest,
    Oldest,
    Name,
    NameDesc,
}

impl SortOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Oldest => "oldest",
            Self::Name => "name",
            Self::NameDesc => "name_desc",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "oldest" => Self::Oldest,
            "name" => Self::Name,
            "name_desc" => Self::NameDesc,
            _ => Self::Newest,
        }
    }

    pub fn ascending(self) -> bool {
        matches!(self, Self::Oldest | Self::Name)
    }

    pub fn by_name(self) -> bool {
        matches!(self, Self::Name | Self::NameDesc)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCursor {
    pub(crate) source: Option<SourceId>,
    pub(crate) time: i64,
    pub(crate) key: AssetKey,
    #[serde(default)]
    pub(crate) order: SortOrder,
    /// Sort key of the boundary item in name order.
    #[serde(default)]
    pub(crate) name: String,
}

impl TimelineCursor {
    pub fn encode(&self) -> Result<String> {
        Ok(hex::encode(serde_json::to_vec(self)?))
    }

    pub fn decode(token: &str) -> Result<Self> {
        if token.len() > 16_384 {
            return Err(Error::Invalid("cursor is too long".into()));
        }
        let bytes = hex::decode(token).map_err(|_| Error::Invalid("invalid cursor".into()))?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetPage {
    pub items: Vec<Asset>,
    pub next: Option<TimelineCursor>,
}

/// Both directions are keyset seeks, so a virtualized host can drop old pages.
#[derive(Debug, Clone)]
pub struct TimelineWindow {
    pub items: Vec<Asset>,
    pub next: Option<TimelineCursor>,
    pub previous: Option<TimelineCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanCheckpoint {
    pub generation: u64,
    pub cursor: Option<String>,
    pub complete: bool,
    pub indexed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogStats {
    pub sources: u64,
    pub assets: u64,
    pub retained_revisions: u64,
}
