use crate::ProtonFolderSource;
use proton_drive_rs::{Node, ProtonDriveClient};
use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::*;
use proton_gallery_rs::source::MediaSource;
use proton_gallery_rs::{CancellationToken, Error, Result, indexer};
use std::collections::HashSet;

/// Metadata-only depth-first crawl. Commits the current album before visiting
/// its children. Every folder has independent resume state; missing nodes do
/// not delete old catalog entries. This is not a durable background-job lease.
pub async fn scan_gallery(
    catalog: &Catalog,
    client: ProtonDriveClient,
    account: AccountId,
    root: Node,
    restart: bool,
    cancel: &CancellationToken,
    mut on_album: impl FnMut(&SourceSpec, &ScanCheckpoint),
) -> Result<SourceId> {
    let root_source = ProtonFolderSource::new(client.clone(), account.clone(), &root)?;
    let gallery = root_source.spec().id.clone();
    let mut stack = vec![(root, None)];
    let mut visited = HashSet::new();
    while let Some((folder, parent)) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let uid = folder.uid.to_string();
        if !visited.insert(uid) {
            return Err(Error::Invalid(
                "Drive folder cycle or duplicate parent".into(),
            ));
        }
        if visited.len() > 10_000 {
            return Err(Error::Invalid(
                "gallery scan exceeds 10000 albums; select smaller roots".into(),
            ));
        }
        let source = ProtonFolderSource::new(client.clone(), account.clone(), &folder)?;
        catalog
            .register_album(gallery.clone(), source.spec().clone(), parent)
            .await?;
        indexer::scan(catalog, &source, restart, cancel, |checkpoint| {
            on_album(source.spec(), checkpoint)
        })
        .await?;
        let mut children = source.subfolders(cancel).await?;
        children.sort_by(|a, b| b.name.cmp(&a.name));
        stack.extend(
            children
                .into_iter()
                .map(|n| (n, Some(source.spec().id.clone()))),
        );
    }
    Ok(gallery)
}
