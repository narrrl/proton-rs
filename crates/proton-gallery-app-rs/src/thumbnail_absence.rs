//! Photos that Proton has said have no thumbnail, so a grid that comes back
//! to them goes straight to its own fallback instead of asking again.
//!
//! An entry is only made from a definite answer: a rendition request that
//! returned no rendition, or a key [`App::find_missing_thumbnails`] returned.
//! A failed request, a timeout or a key left out of a batch is no evidence,
//! and leaves the photo to be asked about as before.
//!
//! The record is kept in memory only, for [`TTL`], and holds at most
//! [`CAPACITY`] photos. A Photos event for a node drops its entry, a gap in
//! the event feed or a new session drops them all. A stale entry costs no
//! more than one fallback: the fallback reads the photo's current original.
//!
//! [`App::find_missing_thumbnails`]: crate::App::find_missing_thumbnails

use proton_drive_rs::ThumbnailType;
use proton_gallery_rs::model::AssetKey;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long an answer is trusted.
pub const TTL: Duration = Duration::from_secs(30 * 60);
/// Most photos remembered at once; the oldest answer goes first.
pub const CAPACITY: usize = 4096;

#[derive(Clone, Copy, Debug)]
struct Entry {
    at: Instant,
    thumbnail: bool,
    preview: bool,
}

#[derive(Debug, Default)]
pub struct ThumbnailAbsence {
    entries: HashMap<AssetKey, Entry>,
}

impl ThumbnailAbsence {
    /// Whether Proton said, within [`TTL`] of `now`, that `key` has no
    /// rendition of `kind`.
    pub fn known_absent(&self, key: &AssetKey, kind: ThumbnailType, now: Instant) -> bool {
        self.entries.get(key).is_some_and(|e| {
            now.duration_since(e.at) < TTL
                && match kind {
                    ThumbnailType::Thumbnail => e.thumbnail,
                    ThumbnailType::Preview => e.preview,
                }
        })
    }

    /// Record Proton's answer for one rendition of `key`. A rendition that
    /// exists drops whatever was known about the photo.
    pub fn record(&mut self, key: &AssetKey, kind: ThumbnailType, present: bool, now: Instant) {
        if present {
            self.entries.remove(key);
            return;
        }
        let fresh = Entry {
            at: now,
            thumbnail: false,
            preview: false,
        };
        let entry = match self.entries.get_mut(key) {
            Some(e) if now.duration_since(e.at) < TTL => e,
            Some(e) => {
                *e = fresh;
                e
            }
            None => {
                self.make_room(now);
                self.entries.entry(key.clone()).or_insert(fresh)
            }
        };
        match kind {
            ThumbnailType::Thumbnail => entry.thumbnail = true,
            ThumbnailType::Preview => entry.preview = true,
        }
    }

    /// Record that `key` has no rendition of either kind.
    pub fn record_missing(&mut self, key: &AssetKey, now: Instant) {
        self.record(key, ThumbnailType::Thumbnail, false, now);
        self.record(key, ThumbnailType::Preview, false, now);
    }

    /// Drop what is known about the node `node` on `volume`, which changed.
    pub fn forget_node(&mut self, volume: &str, node: &str) {
        self.entries
            .retain(|k, _| k.volume.as_str() != volume || k.node.as_str() != node);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn make_room(&mut self, now: Instant) {
        if self.entries.len() < CAPACITY {
            return;
        }
        self.entries.retain(|_, e| now.duration_since(e.at) < TTL);
        while self.entries.len() >= CAPACITY {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_gallery_rs::model::{AccountId, NodeId, VolumeId};

    fn key(node: &str) -> AssetKey {
        AssetKey {
            account: AccountId::new("account").unwrap(),
            volume: VolumeId::new("volume").unwrap(),
            node: NodeId::new(node).unwrap(),
        }
    }

    #[test]
    fn an_answer_is_kept_per_rendition_until_it_expires() {
        let mut absent = ThumbnailAbsence::default();
        let now = Instant::now();
        absent.record(&key("a"), ThumbnailType::Thumbnail, false, now);
        assert!(absent.known_absent(&key("a"), ThumbnailType::Thumbnail, now));
        assert!(!absent.known_absent(&key("a"), ThumbnailType::Preview, now));
        assert!(!absent.known_absent(&key("b"), ThumbnailType::Thumbnail, now));
        absent.record(&key("a"), ThumbnailType::Preview, false, now);
        assert!(absent.known_absent(&key("a"), ThumbnailType::Preview, now));
        let later = now + TTL;
        assert!(!absent.known_absent(&key("a"), ThumbnailType::Thumbnail, later));
    }

    #[test]
    fn a_rendition_found_or_a_change_drops_the_entry() {
        let mut absent = ThumbnailAbsence::default();
        let now = Instant::now();
        absent.record_missing(&key("a"), now);
        absent.record_missing(&key("b"), now);
        absent.record(&key("a"), ThumbnailType::Preview, true, now);
        assert!(!absent.known_absent(&key("a"), ThumbnailType::Thumbnail, now));
        absent.forget_node("volume", "b");
        assert!(absent.is_empty());
    }

    #[test]
    fn an_expired_entry_starts_over_when_answered_again() {
        let mut absent = ThumbnailAbsence::default();
        let now = Instant::now();
        absent.record(&key("a"), ThumbnailType::Preview, false, now);
        let later = now + TTL;
        absent.record(&key("a"), ThumbnailType::Thumbnail, false, later);
        assert!(absent.known_absent(&key("a"), ThumbnailType::Thumbnail, later));
        assert!(!absent.known_absent(&key("a"), ThumbnailType::Preview, later));
    }

    #[test]
    fn the_record_never_grows_past_its_capacity() {
        let mut absent = ThumbnailAbsence::default();
        let start = Instant::now();
        for n in 0..(CAPACITY + 10) {
            absent.record_missing(
                &key(&format!("n{n}")),
                start + Duration::from_millis(n as u64),
            );
        }
        assert_eq!(absent.len(), CAPACITY);
        let last = start + Duration::from_millis((CAPACITY + 9) as u64);
        assert!(absent.known_absent(
            &key(&format!("n{}", CAPACITY + 9)),
            ThumbnailType::Thumbnail,
            last
        ));
        assert!(!absent.known_absent(&key("n0"), ThumbnailType::Thumbnail, last));
    }
}
