//! A persistent cache of small encoded pictures: thumbnails, previews, posters.
//!
//! Each Proton app kept its own: Proton Stream wrote posters and thumbnails
//! with no budget and no atomic writes, Proton Gallery's desktop kept none and
//! downloaded every thumbnail again on each launch, and the Android gallery
//! wrote its own in Kotlin. This is one cache with the [`DiskCache`] guarantees
//! (crash-safe entries, byte budget, least recently used eviction, adoption of
//! the previous run's entries), keyed the way pictures are looked up.
//!
//! The cache stores *encoded* bytes (JPEG, PNG, WebP). Decoded pixels belong to
//! the frontend's own memory cache, which knows its texture type.
//!
//! Entries are stored unencrypted, like the blocks of [`DiskCache`]. Pictures
//! the user marked private (a hidden album) must not be put here at all.

use std::path::PathBuf;

use crate::Result;
use crate::disk::{DiskCache, DiskCacheConfig, DiskStats, hex};

/// Identifies one cached picture.
///
/// * `source`: what the picture is of. A node UID, a URL, anything stable. It
///   is hashed and never written to disk.
/// * `version`: what makes an older copy wrong, such as a revision id or a
///   modification time. A new version is a different entry, so stale bytes are
///   never served; the old one ages out under the budget. Use `""` for sources
///   that never change.
/// * `variant`: which picture of that source, such as `"thumbnail"`,
///   `"preview"` or `"w640"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageKey<'a> {
    pub source: &'a str,
    pub version: &'a str,
    pub variant: &'a str,
}

impl<'a> ImageKey<'a> {
    pub const fn new(source: &'a str, version: &'a str, variant: &'a str) -> Self {
        Self {
            source,
            version,
            variant,
        }
    }
}

/// A byte-budgeted, crash-safe picture cache on disk. See the module note.
pub struct ImageCache {
    disk: DiskCache,
}

impl ImageCache {
    /// 256 MiB: tens of thousands of grid thumbnails.
    pub const DEFAULT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

    /// Open the cache in `root` with [`ImageCache::DEFAULT_BUDGET_BYTES`],
    /// adopting what a previous run left there.
    pub async fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with(DiskCacheConfig::new(root).with_budget(Self::DEFAULT_BUDGET_BYTES)).await
    }

    /// Open the cache with an explicit root and budget.
    ///
    /// Give each kind of picture that is invalidated differently (Proton
    /// thumbnails, provider posters) its own root, so one can be cleared
    /// without the other.
    pub async fn open_with(config: DiskCacheConfig) -> Result<Self> {
        Ok(Self {
            disk: DiskCache::open(config).await?,
        })
    }

    /// The cached bytes, or `None` when absent or failing validation.
    pub async fn get(&self, key: &ImageKey<'_>) -> Option<Vec<u8>> {
        self.disk.get_at(self.path(key)).await
    }

    /// Store a picture. Failures are logged and swallowed, as for
    /// [`DiskCache::put`].
    pub async fn put(&self, key: &ImageKey<'_>, bytes: &[u8]) {
        self.disk.put_at(self.path(key), bytes).await;
    }

    /// Where `key` is stored, for frontends that load pictures from a file.
    /// Only meaningful after [`ImageCache::get`] or [`ImageCache::put`]
    /// succeeded; the file may be evicted at any time after that.
    pub fn path(&self, key: &ImageKey<'_>) -> PathBuf {
        self.disk
            .entry_path(key.source, key.version, &entry_name(key.variant))
    }

    pub async fn set_budget(&self, bytes: u64) {
        self.disk.set_budget(bytes).await;
    }

    pub async fn clear(&self) {
        self.disk.clear().await;
    }

    pub fn stats(&self) -> DiskStats {
        self.disk.stats()
    }
}

/// The file name for a variant: `i-{variant}` when the variant is a short
/// plain name, otherwise `i-{hash}`, so no variant can add a path separator or
/// an extension (the startup scan reads names with an extension as sidecars).
fn entry_name(variant: &str) -> String {
    let plain = !variant.is_empty()
        && variant.len() <= 64
        && variant
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if plain {
        format!("i-{variant}")
    } else {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(variant.as_bytes());
        format!("i-{}", hex(&digest[..16]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("proton-media-images-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[tokio::test]
    async fn a_stored_picture_reads_back() {
        let root = temp_root("roundtrip");
        let cache = ImageCache::open(&root).await.expect("open");
        let key = ImageKey::new("node-1", "rev-1", "thumbnail");
        assert_eq!(cache.get(&key).await, None);
        cache.put(&key, b"jpeg bytes").await;
        assert_eq!(cache.get(&key).await.as_deref(), Some(&b"jpeg bytes"[..]));
        assert!(cache.path(&key).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_new_version_does_not_see_the_old_picture() {
        let root = temp_root("version");
        let cache = ImageCache::open(&root).await.expect("open");
        cache
            .put(&ImageKey::new("node-1", "rev-1", "thumbnail"), b"old")
            .await;
        assert_eq!(
            cache
                .get(&ImageKey::new("node-1", "rev-2", "thumbnail"))
                .await,
            None
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn variants_of_one_source_are_separate_entries() {
        let root = temp_root("variants");
        let cache = ImageCache::open(&root).await.expect("open");
        let thumbnail = ImageKey::new("node-1", "", "thumbnail");
        let preview = ImageKey::new("node-1", "", "preview");
        cache.put(&thumbnail, b"small").await;
        cache.put(&preview, b"large").await;
        assert_eq!(cache.get(&thumbnail).await.as_deref(), Some(&b"small"[..]));
        assert_eq!(cache.get(&preview).await.as_deref(), Some(&b"large"[..]));
        assert_eq!(
            cache.path(&thumbnail).parent(),
            cache.path(&preview).parent()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn entries_survive_a_reopen() {
        let root = temp_root("reopen");
        let key = ImageKey::new("https://example.org/poster.jpg", "", "w640");
        {
            let cache = ImageCache::open(&root).await.expect("open");
            cache.put(&key, b"poster").await;
        }
        let cache = ImageCache::open(&root).await.expect("reopen");
        assert_eq!(cache.stats().entries, 1);
        assert_eq!(cache.get(&key).await.as_deref(), Some(&b"poster"[..]));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn variant_names_never_leave_their_directory() {
        assert_eq!(entry_name("w640"), "i-w640");
        for hostile in ["../escape", "a/b", "logo.png", "", "ü"] {
            let name = entry_name(hostile);
            assert!(
                name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                "{hostile:?} became {name:?}"
            );
        }
        assert_ne!(entry_name("a/b"), entry_name("a.b"));
    }
}
