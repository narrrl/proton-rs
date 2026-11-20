//! Deterministic, account-free fixtures. No full-library allocation.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::bytes::RevisionDescriptor;
use crate::model::*;
use crate::source::{BlockSource, MediaSource, SourceCapabilities, SourcePage};
use crate::{CancellationToken, Error, Result};

pub struct SyntheticSource {
    spec: SourceSpec,
    count: usize,
    delay: Duration,
    fail_at: Option<usize>,
}

impl SyntheticSource {
    pub fn new(count: usize) -> Result<Self> {
        if count > 1_000_000 {
            return Err(Error::Invalid(
                "demo library is limited to one million assets".into(),
            ));
        }
        Ok(Self {
            spec: SourceSpec {
                id: SourceId::new("demo")?,
                account: AccountId::new("synthetic")?,
                volume: VolumeId::new("demo-volume")?,
                root: NodeId::new("root")?,
                label: "Demo library".into(),
            },
            count,
            delay: Duration::ZERO,
            fail_at: None,
        })
    }

    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
    pub fn fail_at(mut self, offset: usize) -> Self {
        self.fail_at = Some(offset);
        self
    }

    pub fn asset(&self, index: usize) -> Result<Asset> {
        if index >= self.count {
            return Err(Error::NotFound);
        }
        let video = index.is_multiple_of(5);
        Ok(Asset {
            key: AssetKey {
                account: self.spec.account.clone(),
                volume: self.spec.volume.clone(),
                node: NodeId::new(format!("asset-{index:08}"))?,
            },
            name: format!("media-{index:08}.{}", if video { "mp4" } else { "jpg" }),
            kind: if video {
                MediaKind::Video
            } else {
                MediaKind::Image
            },
            mime: if video { "video/mp4" } else { "image/jpeg" }.into(),
            revision: Some(RevisionId::new("demo-v1")?),
            size_bytes: None,
            // Repeated timestamps exercise complete-key pagination.
            sort_time: 1_700_000_000 - (index / 8) as i64,
            favorite: false,
        })
    }
}

#[async_trait]
impl MediaSource for SyntheticSource {
    fn spec(&self) -> &SourceSpec {
        &self.spec
    }
    fn capabilities(&self) -> SourceCapabilities {
        SourceCapabilities {
            incremental_listing: true,
            revision_reads: true,
        }
    }

    async fn list_page(
        &self,
        cursor: Option<&str>,
        limit: PageSize,
        cancel: &CancellationToken,
    ) -> Result<SourcePage> {
        let offset = cursor
            .unwrap_or("0")
            .parse::<usize>()
            .map_err(|_| Error::Invalid("invalid synthetic cursor".into()))?;
        if offset > self.count {
            return Err(Error::Invalid("synthetic cursor exceeds library".into()));
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = tokio::time::sleep(self.delay) => {},
        }
        if self.fail_at.is_some_and(|failure| offset >= failure) {
            return Err(Error::Remote("injected source failure"));
        }
        let end = (offset + limit.get()).min(self.count);
        Ok(SourcePage {
            assets: (offset..end)
                .map(|index| self.asset(index))
                .collect::<Result<_>>()?,
            next: (end < self.count).then(|| end.to_string()),
        })
    }
}

/// Patterned bytes, deliberately not a playable video or image.
pub struct SyntheticBlocks {
    descriptor: RevisionDescriptor,
    calls: AtomicU64,
    delay: Duration,
}

impl SyntheticBlocks {
    pub fn new(descriptor: RevisionDescriptor) -> Self {
        Self {
            descriptor,
            calls: AtomicU64::new(0),
            delay: Duration::ZERO,
        }
    }
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
    pub fn expected(offset: u64, length: usize) -> Vec<u8> {
        (offset..offset + length as u64)
            .map(|byte| (byte % 251) as u8)
            .collect()
    }
}

#[async_trait]
impl BlockSource for SyntheticBlocks {
    fn descriptor(&self) -> &RevisionDescriptor {
        &self.descriptor
    }

    async fn read_block(&self, index: usize, cancel: &CancellationToken) -> Result<Vec<u8>> {
        let size = *self
            .descriptor
            .block_sizes()
            .get(index)
            .ok_or_else(|| Error::Invalid("block index out of bounds".into()))?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = tokio::time::sleep(self.delay) => {},
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(Self::expected(
            self.descriptor.offset_of(index).ok_or(Error::NotFound)?,
            size as usize,
        ))
    }
}
