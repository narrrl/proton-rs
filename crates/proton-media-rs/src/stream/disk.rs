//! The disk cache, keyed by [`BlockKey`]: the file key's `Display` form is the
//! file identity, so a caller that keys by a stable string (proton-stream uses
//! the node UID) adopts caches it wrote before this moved here.

use crate::{BlockId, DiskCacheConfig, DiskStats, Result};

use super::FileKey;
use super::ring::BlockKey;

pub(crate) struct DiskCache(crate::DiskCache);

impl DiskCache {
    /// Open the cache, adopting whatever a previous run left behind.
    pub(crate) async fn open(config: DiskCacheConfig) -> Result<Self> {
        Ok(Self(crate::DiskCache::open(config).await?))
    }

    /// Read a block back, or `None` when it is absent or fails validation.
    pub(crate) async fn get<K: FileKey>(&self, key: &BlockKey<K>) -> Option<Vec<u8>> {
        let file = key.file.to_string();
        self.0.get(&id(&file, key)).await
    }

    /// Store a block. Failures are logged and swallowed.
    pub(crate) async fn put<K: FileKey>(&self, key: &BlockKey<K>, block: &[u8]) {
        let file = key.file.to_string();
        self.0.put(&id(&file, key), block).await
    }

    pub(crate) async fn set_budget(&self, bytes: u64) {
        self.0.set_budget(bytes).await
    }

    pub(crate) async fn clear(&self) {
        self.0.clear().await
    }

    pub(crate) fn stats(&self) -> DiskStats {
        self.0.stats()
    }
}

fn id<'a, K>(file: &'a str, key: &'a BlockKey<K>) -> BlockId<'a> {
    BlockId {
        file,
        revision: &key.revision,
        index: key.index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key's display form is the file identity, so a block cached by an
    /// earlier layout under that same string is still found.
    #[tokio::test]
    async fn blocks_are_keyed_by_the_files_display_form_and_revision() {
        let root = std::env::temp_dir().join(format!("pmr-stream-disk-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let file = "vol~a".to_string();
        let key = BlockKey::new(&file, "rev1", 2);

        let shared = crate::DiskCache::open(DiskCacheConfig::new(&root))
            .await
            .unwrap();
        shared.put(&id(&file, &key), &[3_u8; 64]).await;
        drop(shared);

        let cache = DiskCache::open(DiskCacheConfig::new(&root)).await.unwrap();
        assert_eq!(cache.get(&key).await, Some(vec![3_u8; 64]));
        assert!(cache.get(&BlockKey::new(&file, "rev2", 2)).await.is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
