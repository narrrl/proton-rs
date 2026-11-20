use async_trait::async_trait;

use crate::bytes::RevisionDescriptor;
use crate::model::{Asset, PageSize, SourceSpec};
use crate::{CancellationToken, Result};

#[derive(Debug, Clone, Copy)]
pub struct SourceCapabilities {
    /// True only when the first page does not wait for the complete ID listing.
    pub incremental_listing: bool,
    pub revision_reads: bool,
}

#[derive(Debug, Clone)]
pub struct SourcePage {
    pub assets: Vec<Asset>,
    pub next: Option<String>,
}

#[async_trait]
pub trait MediaSource: Send + Sync {
    fn spec(&self) -> &SourceSpec;
    fn kind(&self) -> crate::library::SourceKind {
        crate::library::SourceKind::Other
    }
    fn capabilities(&self) -> SourceCapabilities;
    async fn list_page(
        &self,
        cursor: Option<&str>,
        limit: PageSize,
        cancel: &CancellationToken,
    ) -> Result<SourcePage>;
}

#[async_trait]
pub trait BlockSource: Send + Sync {
    fn descriptor(&self) -> &RevisionDescriptor;
    async fn read_block(&self, index: usize, cancel: &CancellationToken) -> Result<Vec<u8>>;
}
