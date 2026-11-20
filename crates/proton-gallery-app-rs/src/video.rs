//! Proton videos that play while they download. The player asks for byte
//! ranges; each range comes from one block of the revision, decrypted in
//! memory. Nothing is written to disk, so videos in hidden albums stream the
//! same way. A few blocks around the play position stay cached, and the next
//! ones are fetched ahead of it.
//!
//! A range read cannot check the signature over the whole file's manifest,
//! which the full download does. Each block is still decrypted and checked
//! on its own.
use super::*;
use proton_drive_rs::RevisionReader;
use proton_gallery_rs::model::AssetKey;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{OnceCell, Semaphore};

/// Decrypted blocks kept per stream: about 24 MiB at Proton's 4 MiB blocks.
const KEPT_BLOCKS: usize = 6;
/// Blocks fetched ahead of the one being read.
const READ_AHEAD: usize = 2;

type Block = Arc<OnceCell<Arc<Vec<u8>>>>;

pub struct VideoStream {
    reader: Arc<RevisionReader>,
    /// Plaintext offset where each block starts, then the total length.
    starts: Vec<u64>,
    blocks: Mutex<HashMap<usize, Block>>,
    runtime: tokio::runtime::Handle,
    read_ahead: Arc<Semaphore>,
    cancel: proton_gallery_rs::CancellationToken,
}

/// Where each block starts, followed by the total length.
fn block_starts(sizes: &[u64]) -> Vec<u64> {
    let mut starts = Vec::with_capacity(sizes.len() + 1);
    let mut at = 0u64;
    for size in sizes {
        starts.push(at);
        at = at.saturating_add(*size);
    }
    starts.push(at);
    starts
}

/// The block holding `offset`, or `None` at or past the end.
fn block_at(starts: &[u64], offset: u64) -> Option<usize> {
    let total = *starts.last()?;
    if offset >= total {
        return None;
    }
    // The last block that starts at or before `offset`; skips empty blocks.
    Some(starts.partition_point(|&start| start <= offset) - 1)
}

/// The blocks to drop so that at most `keep` remain: those farthest from `near`.
fn evictions(cached: impl Iterator<Item = usize>, near: usize, keep: usize) -> Vec<usize> {
    let mut cached: Vec<usize> = cached.collect();
    if cached.len() <= keep {
        return Vec::new();
    }
    // Blocks behind the play position go before blocks ahead of it.
    cached.sort_by_key(|&index| {
        std::cmp::Reverse(if index < near {
            (near - index) * 2 + 1
        } else {
            index - near
        })
    });
    cached.truncate(cached.len() - keep);
    cached
}

impl VideoStream {
    fn cell(&self, index: usize) -> Block {
        let mut blocks = self.blocks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cell) = blocks.get(&index) {
            return cell.clone();
        }
        let cell = Block::default();
        blocks.insert(index, cell.clone());
        for old in evictions(blocks.keys().copied(), index, KEPT_BLOCKS) {
            blocks.remove(&old);
        }
        cell
    }

    async fn block(&self, index: usize) -> Result<Arc<Vec<u8>>> {
        let start = self.starts[index];
        let length = self.starts[index + 1] - start;
        let cell = self.cell(index);
        let bytes = cell
            .get_or_try_init(|| async {
                tokio::select! { biased;
                    _ = self.cancel.cancelled() => Err(Error::Cancelled),
                    bytes = self.reader.read_at(start, length) => {
                        let bytes = bytes.map_err(|e| {
                            tracing::warn!("video block failed: {}", proton_gallery_drive_rs::auth::cause_chain(&e));
                            Error::message("Couldn’t load the video. Check your connection and try again.")
                        })?;
                        if bytes.len() as u64 != length {
                            return Err(proton_gallery_rs::Error::RevisionMismatch.into());
                        }
                        Ok(Arc::new(bytes))
                    }
                }
            })
            .await?;
        Ok(bytes.clone())
    }

    fn read_ahead(self: &Arc<Self>, index: usize) {
        let last = self.starts.len() - 2;
        for next in index + 1..=(index + READ_AHEAD).min(last) {
            let present = self
                .blocks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&next);
            if !present {
                let Ok(permit) = self.read_ahead.clone().try_acquire_owned() else {
                    break;
                };
                let stream = self.clone();
                self.runtime.spawn(async move {
                    let _permit = permit;
                    let _ = stream.block(next).await;
                });
            }
        }
    }
}

impl VideoStream {
    /// Open a seekable reader around an already authorized revision.
    pub fn from_revision(
        reader: RevisionReader,
        cancel: proton_gallery_rs::CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            starts: block_starts(reader.block_sizes()),
            reader: Arc::new(reader),
            blocks: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
            read_ahead: Arc::new(Semaphore::new(READ_AHEAD)),
            cancel: cancel.child_token(),
        })
    }

    /// The video's length in bytes.
    pub fn length(&self) -> u64 {
        *self.starts.last().unwrap_or(&0)
    }

    /// Up to `length` bytes from `offset`, never past the end of one block.
    /// Returns an empty buffer at EOF.
    pub async fn read(self: &Arc<Self>, offset: u64, length: u32) -> Result<Vec<u8>> {
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let Some(index) = block_at(&self.starts, offset) else {
            return Ok(Vec::new());
        };
        if length == 0 {
            return Ok(Vec::new());
        }
        let bytes = self.block(index).await?;
        if self.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        self.read_ahead(index);
        let from = (offset - self.starts[index]) as usize;
        let to = from.saturating_add(length as usize).min(bytes.len());
        Ok(bytes[from..to].to_vec())
    }

    /// Stop fetching and forget the cached blocks; reads after this fail.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.blocks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl App {
    /// Fetch the block table for a Proton video. The player reads the blocks on demand.
    pub async fn stream_video(&self, key: &AssetKey) -> Result<Arc<VideoStream>> {
        let _operations = self.operations.read().await;
        let account = self.auth.account().await?;
        self.owns(&account.id, key)?;
        let uid = proton_gallery_drive_rs::node_uid(key);
        let reader = tokio::select! { biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            reader = account.drive.open_revision(&uid) => reader.map_err(|e| {
                tracing::warn!("video open failed: {}", proton_gallery_drive_rs::auth::cause_chain(&e));
                Error::message("Couldn’t open the video. Check your connection and try again.")
            })?,
        };
        Ok(VideoStream::from_revision(reader, account.cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_map_to_their_block() {
        let starts = block_starts(&[4, 4, 2]);
        assert_eq!(starts, vec![0, 4, 8, 10]);
        assert_eq!(block_at(&starts, 0), Some(0));
        assert_eq!(block_at(&starts, 3), Some(0));
        assert_eq!(block_at(&starts, 4), Some(1));
        assert_eq!(block_at(&starts, 9), Some(2));
        assert_eq!(block_at(&starts, 10), None);
        assert_eq!(block_at(&block_starts(&[]), 0), None);
    }

    #[test]
    fn empty_blocks_are_skipped() {
        let starts = block_starts(&[4, 0, 4]);
        assert_eq!(block_at(&starts, 4), Some(2));
    }

    #[test]
    fn eviction_keeps_blocks_near_the_play_position() {
        let mut dropped = evictions([0, 1, 2, 3, 4, 5, 6, 7].into_iter(), 4, 6);
        dropped.sort();
        assert_eq!(dropped, vec![0, 1]);
        // Behind counts double: with 3 kept around 5, block 4 goes before 7.
        let mut dropped = evictions([3, 4, 5, 6, 7].into_iter(), 5, 3);
        dropped.sort();
        assert_eq!(dropped, vec![3, 4]);
        assert!(evictions([1, 2].into_iter(), 1, 6).is_empty());
    }
}
