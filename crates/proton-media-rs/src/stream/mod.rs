//! Seekable, cached byte streams over files stored as independently readable
//! blocks — a Proton revision, or a finished download of one.
//!
//! ```text
//!   demuxer             RangeStream          BlockRing → DiskCache → BlockSource
//!   (small, forward, →  byte range →    →    memory      disk        network
//!    seeking reads)     blocks              (128 MiB)   (4 GiB)
//! ```
//!
//! * [`StreamSource`] opens files and keeps the open ones in an LRU.
//! * [`RangeStream`] answers byte ranges, reading ahead of the reader and
//!   cancelling that read-ahead on a seek.
//! * The ring and the disk cache are byte-budgeted, shared across streams, and
//!   keyed by revision id so a resealed file can never serve stale bytes.
//!
//! Files are identified by any [`FileKey`]; its `Display` form names the file
//! in the disk cache, so it must be stable across runs. Everything here runs
//! against a [`BlockSource`], which is what makes the caching and read-ahead
//! testable without an account or a network. Only `RevisionBlocks`, behind the
//! `drive` feature, talks to the SDK.

use std::fmt::{Debug, Display};
use std::hash::Hash;

mod disk;
#[cfg(feature = "drive")]
mod drive;
mod file;
mod open;
mod ring;
mod source;
mod video;

#[cfg(feature = "drive")]
pub use drive::RevisionBlocks;
pub use file::FileBlocks;
pub use open::{RevisionOpener, StreamConfig, StreamSource};
pub use ring::{DEFAULT_RING_BYTES, RingStats};
pub use source::{BlockSource, MemoryBlocks, SharedBlocks};
pub use video::{COPY_BLOCKS_IN_FLIGHT, DEFAULT_READAHEAD_BLOCKS, RangeStream, StreamStats};

/// What identifies a file to the stream layer: cheap to clone, hashable, and
/// displayable as a name that is stable across runs (the disk cache key).
pub trait FileKey: Clone + Eq + Hash + Debug + Display + Send + Sync + 'static {}

impl<T: Clone + Eq + Hash + Debug + Display + Send + Sync + 'static> FileKey for T {}
