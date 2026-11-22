//! Opening streams, and keeping the open ones around.
//!
//! A cold open is expensive: link details, an ancestor-key unlock (an S2K
//! derivation, tens of milliseconds), then a revision listing — all before the
//! first byte. Two things make that cost once instead of repeatedly:
//!
//! * an **LRU of open streams**, so going back to the episode you paused, or
//!   scrubbing after the player reopened its demuxer, is instant;
//! * **single-flight on the open itself**, because the UI's "play" click and the
//!   player's first read arrive milliseconds apart and would otherwise both pay
//!   for the same handshake, then race to insert into the LRU.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use lru::LruCache;

use super::FileKey;
use super::disk::DiskCache;
use super::ring::{BlockRing, DEFAULT_RING_BYTES, RingStats, non_zero};
use super::source::SharedBlocks;
use super::video::{DEFAULT_READAHEAD_BLOCKS, RangeStream};
use crate::{DiskCacheConfig, DiskStats, Result, SingleFlight};

/// How the source turns a file into blocks.
///
/// A trait so the cache, ring and read-ahead can be exercised without an
/// account. `scope` is whatever the file is reached through (a share, a
/// volume): the same file through two scopes is two streams.
#[async_trait]
pub trait RevisionOpener<K: FileKey>: Send + Sync + 'static {
    /// Open the active revision of `file` inside `scope`.
    async fn open(&self, scope: &str, file: &K) -> Result<SharedBlocks>;
}

/// How much memory, how much disk, and how far ahead to read.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Resident block budget, shared by every open stream.
    pub ring_bytes: u64,
    /// Blocks to pull ahead of the reader. Zero disables read-ahead.
    pub readahead_blocks: usize,
    /// How many opened streams to keep. Cheap to hold — the bytes live in the
    /// ring, not here — so this is generous.
    pub max_open_streams: usize,
    /// The on-disk block cache, when enabled.
    pub disk_cache: Option<DiskCacheConfig>,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            ring_bytes: DEFAULT_RING_BYTES,
            readahead_blocks: DEFAULT_READAHEAD_BLOCKS,
            max_open_streams: 64,
            disk_cache: None,
        }
    }
}

impl StreamConfig {
    pub fn with_disk_cache(mut self, cache: DiskCacheConfig) -> Self {
        self.disk_cache = Some(cache);
        self
    }

    pub fn with_readahead(mut self, blocks: usize) -> Self {
        self.readahead_blocks = blocks;
        self
    }

    pub fn with_ring_bytes(mut self, bytes: u64) -> Self {
        self.ring_bytes = bytes;
        self
    }
}

/// Identifies an open stream. The share is part of the key: the same file can
/// legitimately be reachable through two different links.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StreamKey<K> {
    share: String,
    file: K,
}

/// Opens and caches [`RangeStream`]s.
pub struct StreamSource<K: FileKey> {
    inner: Arc<Inner<K>>,
}

impl<K: FileKey> Clone for StreamSource<K> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

struct Inner<K: FileKey> {
    opener: Arc<dyn RevisionOpener<K>>,
    ring: Arc<BlockRing<K>>,
    disk: Option<Arc<DiskCache>>,
    readahead: usize,
    open: Mutex<LruCache<StreamKey<K>, RangeStream<K>>>,
    flight: Arc<SingleFlight<StreamKey<K>, RangeStream<K>>>,
}

impl<K: FileKey> StreamSource<K> {
    /// Build a source, opening the disk cache if one is configured.
    pub async fn new(opener: Arc<dyn RevisionOpener<K>>, config: StreamConfig) -> Result<Self> {
        let disk = match config.disk_cache {
            Some(cache) => Some(Arc::new(DiskCache::open(cache).await?)),
            None => None,
        };

        Ok(Self {
            inner: Arc::new(Inner {
                opener,
                ring: Arc::new(BlockRing::new(config.ring_bytes)),
                disk,
                readahead: config.readahead_blocks,
                open: Mutex::new(LruCache::new(non_zero(config.max_open_streams))),
                flight: Arc::new(SingleFlight::new()),
            }),
        })
    }

    /// Open a file for playback, or hand back the stream already open on it.
    pub async fn open(&self, share_id: &str, file: &K) -> Result<RangeStream<K>> {
        let key = StreamKey {
            share: share_id.to_string(),
            file: file.clone(),
        };

        if let Some(stream) = self.inner.lock().get(&key) {
            return Ok(stream.clone());
        }

        let inner = Arc::clone(&self.inner);
        let flight = Arc::clone(&self.inner.flight);
        let share = share_id.to_string();
        let file = file.clone();
        let cached_key = key.clone();

        flight
            .run(key, move || async move {
                let blocks = inner.opener.open(&share, &file).await?;
                let stream = RangeStream::new(
                    file,
                    blocks,
                    Arc::clone(&inner.ring),
                    inner.disk.clone(),
                    inner.readahead,
                );
                // Evicting a stream here only drops the handle; its blocks stay
                // in the ring under the ring's own budget, so a viewer who
                // reopens still gets them.
                inner.lock().put(cached_key, stream.clone());
                Ok(stream)
            })
            .await
    }

    /// Open a file to copy it out whole, rather than to play it.
    ///
    /// Not the playback stream, and not shared with it: its blocks are read
    /// once and written to a file, so caching them in memory only evicts the
    /// blocks a viewer is about to need, and caching them on disk stores every
    /// byte twice while pushing out what was just watched. It does still read
    /// the disk cache — an episode watched and then downloaded is not fetched
    /// again.
    pub async fn open_for_copy(&self, share_id: &str, file: &K) -> Result<RangeStream<K>> {
        let blocks = self.inner.opener.open(share_id, file).await?;
        Ok(RangeStream::copying(
            file.clone(),
            blocks,
            Arc::new(BlockRing::new(0)),
            self.inner.disk.clone(),
        ))
    }

    /// Drop a stream and everything cached for it in memory.
    ///
    /// The disk cache is deliberately untouched: it exists precisely so the next
    /// viewing is cheap.
    pub fn close(&self, share_id: &str, file: &K) {
        let key = StreamKey {
            share: share_id.to_string(),
            file: file.clone(),
        };
        if let Some(stream) = self.inner.lock().pop(&key) {
            // Before the ring is cleared, not after: a prefetch still in flight
            // would otherwise re-insert into the ring this just emptied, and
            // keep downloading for a stream nobody is reading.
            stream.cancel_read_ahead();
            self.inner.ring.forget(file, stream.revision_id());
        }
    }

    /// Ring statistics, across every stream.
    pub fn ring_stats(&self) -> RingStats {
        self.inner.ring.stats()
    }

    /// Disk-cache statistics, when one is configured.
    pub fn disk_stats(&self) -> Option<DiskStats> {
        self.inner.disk.as_ref().map(|disk| disk.stats())
    }

    /// Change the disk cache's budget, evicting at once if it is now over.
    pub async fn set_disk_budget(&self, bytes: u64) {
        if let Some(disk) = &self.inner.disk {
            disk.set_budget(bytes).await;
        }
    }

    /// Empty the disk cache. Streams already open carry on; what they had
    /// cached is fetched again if it is wanted.
    pub async fn clear_disk_cache(&self) {
        if let Some(disk) = &self.inner.disk {
            disk.clear().await;
        }
    }
}

impl<K: FileKey> Inner<K> {
    fn lock(&self) -> std::sync::MutexGuard<'_, LruCache<StreamKey<K>, RangeStream<K>>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::stream::MemoryBlocks;

    fn uid(link: &str) -> String {
        format!("vol~{link}")
    }

    struct CountingOpener {
        opens: AtomicUsize,
        delay: std::time::Duration,
    }

    impl CountingOpener {
        fn new(delay: std::time::Duration) -> Arc<Self> {
            Arc::new(Self {
                opens: AtomicUsize::new(0),
                delay,
            })
        }

        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RevisionOpener<String> for CountingOpener {
        async fn open(&self, _share: &str, _uid: &String) -> Result<SharedBlocks> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(Arc::new(MemoryBlocks::new(
                "rev1",
                vec![vec![1_u8; 64], vec![2_u8; 64]],
            )))
        }
    }

    async fn source(opener: Arc<CountingOpener>) -> StreamSource<String> {
        StreamSource::new(opener, StreamConfig::default().with_readahead(0))
            .await
            .expect("source")
    }

    #[tokio::test]
    async fn an_opened_stream_reads_its_blocks() {
        let source = source(CountingOpener::new(std::time::Duration::ZERO)).await;
        let stream = source.open("share", &uid("a")).await.unwrap();

        assert_eq!(stream.size(), 128);
        assert_eq!(
            stream.read_range(60, 8).await.unwrap(),
            vec![1, 1, 1, 1, 2, 2, 2, 2]
        );
    }

    /// Reopening the episode you paused must not re-run the handshake.
    #[tokio::test]
    async fn reopening_the_same_file_reuses_the_open_stream() {
        let opener = CountingOpener::new(std::time::Duration::ZERO);
        let source = source(Arc::clone(&opener)).await;

        source.open("share", &uid("a")).await.unwrap();
        source.open("share", &uid("a")).await.unwrap();
        source.open("share", &uid("a")).await.unwrap();

        assert_eq!(opener.opens(), 1);
    }

    /// The race the single-flight exists for: the UI click and the player's
    /// first read, milliseconds apart, against a slow open.
    #[tokio::test]
    async fn concurrent_opens_of_one_file_handshake_once() {
        let opener = CountingOpener::new(std::time::Duration::from_millis(40));
        let source = source(Arc::clone(&opener)).await;

        let mut handles = Vec::new();
        for _ in 0..6 {
            let source = source.clone();
            handles.push(tokio::spawn(async move {
                source.open("share", &uid("a")).await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert_eq!(opener.opens(), 1);
    }

    /// The same file behind two links is two streams — the shares may have
    /// different lifetimes and different credentials.
    #[tokio::test]
    async fn the_same_file_in_two_shares_is_two_streams() {
        let opener = CountingOpener::new(std::time::Duration::ZERO);
        let source = source(Arc::clone(&opener)).await;

        source.open("share-one", &uid("a")).await.unwrap();
        source.open("share-two", &uid("a")).await.unwrap();
        assert_eq!(opener.opens(), 2);
    }

    /// Closing must reclaim the memory, or a long session leaks a ring's worth
    /// of every episode watched.
    #[tokio::test]
    async fn closing_a_stream_releases_its_blocks() {
        let source = source(CountingOpener::new(std::time::Duration::ZERO)).await;
        let stream = source.open("share", &uid("a")).await.unwrap();
        stream.read_range(0, 128).await.unwrap();
        assert!(source.ring_stats().resident_bytes > 0);

        source.close("share", &uid("a"));
        assert_eq!(source.ring_stats().resident_bytes, 0);
    }

    /// Beyond the LRU's capacity the oldest handle is dropped; the next open
    /// re-runs the handshake rather than failing.
    #[tokio::test]
    async fn the_open_stream_lru_is_bounded() {
        let opener = CountingOpener::new(std::time::Duration::ZERO);
        let source = StreamSource::new(
            Arc::clone(&opener) as Arc<dyn RevisionOpener<String>>,
            StreamConfig {
                max_open_streams: 2,
                readahead_blocks: 0,
                ..StreamConfig::default()
            },
        )
        .await
        .unwrap();

        for link in ["a", "b", "c"] {
            source.open("share", &uid(link)).await.unwrap();
        }
        // "a" was evicted, so this is a fourth open.
        source.open("share", &uid("a")).await.unwrap();
        assert_eq!(opener.opens(), 4);
    }
}
