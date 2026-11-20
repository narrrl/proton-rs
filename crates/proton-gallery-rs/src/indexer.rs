use crate::catalog::Catalog;
use crate::model::{PageSize, ScanCheckpoint};
use crate::source::MediaSource;
use crate::{CancellationToken, Error, Result};

/// A page is committed before progress is emitted, so the UI can query it at once.
pub async fn scan(
    catalog: &Catalog,
    source: &dyn MediaSource,
    restart: bool,
    cancel: &CancellationToken,
    mut on_progress: impl FnMut(&ScanCheckpoint),
) -> Result<ScanCheckpoint> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    catalog.register_source(source.spec().clone()).await?;
    catalog
        .set_source_kind(source.spec().id.clone(), source.kind())
        .await?;
    let mut checkpoint = catalog
        .begin_scan(source.spec().id.clone(), restart)
        .await?;
    while !checkpoint.complete {
        let page = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            page = source.list_page(checkpoint.cursor.as_deref(), PageSize::default(), cancel) => page?,
        };
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        checkpoint = catalog
            .apply_page(source.spec().clone(), checkpoint, page)
            .await?;
        on_progress(&checkpoint);
    }
    Ok(checkpoint)
}
