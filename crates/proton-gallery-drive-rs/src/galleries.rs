use crate::ProtonFolderSource;
use async_trait::async_trait;
use proton_drive_rs::{Node, ProtonDriveClient};
use proton_drive_tree_rs::{TreeSource, WalkError, WalkOptions, walk};
use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::*;
use proton_gallery_rs::source::MediaSource;
use proton_gallery_rs::{CancellationToken, Error, Result, indexer};
use proton_sdk::error::ProtonError;
use proton_sdk::ids::NodeUid;
use std::collections::HashMap;
use std::sync::Mutex;

/// The most albums one gallery may hold, root included.
const MAX_ALBUMS: usize = 10_000;

/// Metadata-only crawl of a Drive folder tree into a gallery of albums.
///
/// The folder tree is listed first, several folders at a time with dropped
/// listings retried. Then every folder is registered and indexed, root first
/// and parents before their children, each committing as it completes. Every
/// folder has independent resume state; missing nodes do not delete old
/// catalog entries. This is not a durable background-job lease.
pub async fn scan_gallery(
    catalog: &Catalog,
    client: ProtonDriveClient,
    account: AccountId,
    root: Node,
    restart: bool,
    cancel: &CancellationToken,
    mut on_album: impl FnMut(&SourceSpec, &ScanCheckpoint),
) -> Result<SourceId> {
    let gallery = ProtonFolderSource::new(client.clone(), account.clone(), &root)?
        .spec()
        .id
        .clone();
    let tree = FolderTree {
        client: client.clone(),
        listings: Mutex::default(),
    };
    let folders = album_folders(&tree, root, cancel).await?;
    let mut listings = std::mem::take(&mut *tree.listings.lock().expect("listings lock"));

    let mut albums: HashMap<NodeUid, SourceId> = HashMap::new();
    for folder in folders {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let children = listings.remove(&folder.uid).unwrap_or_default();
        let source =
            ProtonFolderSource::listed(client.clone(), account.clone(), &folder, children)?;
        let parent = folder
            .parent_uid
            .as_ref()
            .and_then(|parent| albums.get(parent))
            .cloned();
        catalog
            .register_album(gallery.clone(), source.spec().clone(), parent)
            .await?;
        indexer::scan(catalog, &source, restart, cancel, |checkpoint| {
            on_album(source.spec(), checkpoint)
        })
        .await?;
        albums.insert(folder.uid.clone(), source.spec().id.clone());
    }
    Ok(gallery)
}

/// Every folder of the tree under `root`, root first and by level, each
/// level's folders by name.
async fn album_folders<S>(
    source: &S,
    root: S::Node,
    cancel: &CancellationToken,
) -> Result<Vec<S::Node>>
where
    S: TreeSource,
{
    let options = WalkOptions::default().with_max_folders(MAX_ALBUMS);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        walked = walk(source, root, &options, |_| {}) => walked.map_err(|error| match error {
            WalkError::Source(_) => Error::Remote("could not list Proton folder children"),
            WalkError::TooManyFolders(_) => Error::Invalid(format!(
                "gallery scan exceeds {MAX_ALBUMS} albums; select smaller roots"
            )),
        }),
    }
}

/// A Drive folder tree as a walk sees it: folders only, since an album's
/// files are indexed from its own listing. Keeps each folder's child listing
/// for the indexing that follows.
struct FolderTree {
    client: ProtonDriveClient,
    listings: Mutex<HashMap<NodeUid, Vec<NodeUid>>>,
}

#[async_trait]
impl TreeSource for FolderTree {
    type Node = Node;
    type Error = ProtonError;

    async fn children(&self, folder: &NodeUid) -> std::result::Result<Vec<Node>, ProtonError> {
        let children = self
            .client
            .enumerate_folder_children_node_uids(folder)
            .await?;
        let mut folders = Vec::new();
        for batch in children.chunks(PageSize::MAX) {
            let nodes = self.client.enumerate_nodes(batch).await?;
            folders.extend(nodes.into_iter().filter(|n| n.is_folder() && !n.trashed));
        }
        folders.sort_by(|a, b| a.name.cmp(&b.name));
        self.listings
            .lock()
            .expect("listings lock")
            .insert(folder.clone(), children);
        Ok(folders)
    }

    fn is_retriable(error: &ProtonError) -> bool {
        error.is_retriable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_tree_rs::TreeNode;

    #[derive(Debug, Clone, PartialEq)]
    struct Folder(String);

    fn folder(name: &str) -> Folder {
        Folder(name.to_owned())
    }

    impl TreeNode for Folder {
        type Id = String;
        fn id(&self) -> &String {
            &self.0
        }
        fn is_folder(&self) -> bool {
            true
        }
    }

    /// `root` holds two years, `2025` holds two trips. An endless tree gives
    /// every folder ten more.
    struct Photos {
        endless: bool,
    }

    #[async_trait]
    impl TreeSource for Photos {
        type Node = Folder;
        type Error = ();

        async fn children(&self, parent: &String) -> std::result::Result<Vec<Folder>, ()> {
            if self.endless {
                return Ok((0..10).map(|i| folder(&format!("{parent}/{i}"))).collect());
            }
            Ok(match parent.as_str() {
                "root" => vec![folder("2024"), folder("2025")],
                "2025" => vec![folder("2025/alps"), folder("2025/coast")],
                _ => Vec::new(),
            })
        }
    }

    async fn scan(endless: bool, cancel: &CancellationToken) -> Result<Vec<String>> {
        let folders = album_folders(&Photos { endless }, folder("root"), cancel).await?;
        Ok(folders.into_iter().map(|f| f.0).collect())
    }

    #[tokio::test]
    async fn albums_come_root_first_and_parents_before_children() {
        let names = scan(false, &CancellationToken::new()).await.unwrap();
        assert_eq!(names, ["root", "2024", "2025", "2025/alps", "2025/coast"]);
    }

    #[tokio::test]
    async fn a_tree_past_the_album_cap_is_refused() {
        let error = scan(true, &CancellationToken::new()).await.unwrap_err();
        assert!(
            matches!(&error, Error::Invalid(message) if message.contains("10000 albums")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_scan_lists_nothing() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = scan(false, &cancel).await.unwrap_err();
        assert!(matches!(error, Error::Cancelled), "{error:?}");
    }
}
