//! Folder selection and indexing for Drive galleries.
use super::*;
use proton_gallery_rs::library::Album;
use proton_gallery_rs::model::{PageSize, SourceId};
use proton_sdk::ids::NodeUid;

#[derive(Clone, Debug)]
pub struct FolderChoice {
    pub uid: NodeUid,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct FolderListing {
    pub folder: FolderChoice,
    pub children: Vec<FolderChoice>,
}

fn drive_error(action: &'static str) -> impl Fn(proton_sdk::error::ProtonError) -> Error {
    move |error| {
        tracing::warn!(
            "{action}: {}",
            proton_gallery_drive_rs::auth::cause_chain(&error)
        );
        Error::message(action)
    }
}

impl App {
    pub async fn hidden_drive_albums(&self) -> Result<Vec<Album>> {
        let generation = self.generation();
        let _operations = self.operations.read().await;
        let Some(account) = self.auth.account_id() else {
            return Ok(vec![]);
        };
        self.require_gallery_visibility().await?;
        let albums = self.catalog.hidden_drive_albums(account).await?;
        self.ensure_current(generation)?;
        Ok(albums)
    }

    pub async fn drive_albums(
        &self,
        place: Option<(&str, &str)>,
        unlocked: bool,
    ) -> Result<Vec<Album>> {
        let Some(account) = self.auth.account_id() else {
            return Ok(vec![]);
        };
        self.require_gallery_visibility().await?;
        let (gallery, parent) = if let Some((gallery, parent)) = place {
            let id = self.checked_album(gallery.into(), parent.into()).await?;
            let gallery = SourceId::new(gallery)?;
            if !unlocked
                && self
                    .catalog
                    .drive_album_is_private(gallery.clone(), id.clone())
                    .await?
            {
                return Err(proton_gallery_rs::Error::NotFound.into());
            }
            (Some(gallery), Some(id))
        } else {
            (None, None)
        };
        Ok(self
            .catalog
            .albums_scoped(gallery, parent, Some(account))
            .await?
            .into_iter()
            .filter(|album| unlocked || !album.settings.hidden)
            .collect())
    }

    pub async fn browse_drive_folders(&self, parent: Option<NodeUid>) -> Result<FolderListing> {
        let _operations = self.operations.read().await;
        let account = self.auth.account().await?;
        let drive = account.drive;
        let work = async {
            let folder = if let Some(id) = parent {
                drive
                    .get_node(&id)
                    .await
                    .map_err(drive_error("Couldn’t open this Drive folder."))?
                    .ok_or(proton_gallery_rs::Error::NotFound)?
            } else {
                drive
                    .get_my_files_folder()
                    .await
                    .map_err(drive_error("Couldn’t open My files."))?
            };
            if !folder.is_folder() || folder.trashed {
                return Err(Error::message("Choose an active Drive folder."));
            }
            let mut children = Vec::new();
            let ids = drive
                .enumerate_folder_children_node_uids(&folder.uid)
                .await
                .map_err(drive_error("Couldn’t list this Drive folder."))?;
            for batch in ids.chunks(PageSize::MAX) {
                for node in drive
                    .enumerate_nodes(batch)
                    .await
                    .map_err(drive_error("Couldn’t list this Drive folder."))?
                {
                    if node.is_folder() && !node.trashed {
                        children.push(FolderChoice {
                            uid: node.uid,
                            name: node.name,
                        });
                    }
                }
                if children.len() > 5_000 {
                    return Err(Error::message(
                        "This folder has too many subfolders to browse.",
                    ));
                }
            }
            children.sort_by_cached_key(|child| child.name.to_lowercase());
            Ok(FolderListing {
                folder: FolderChoice {
                    uid: folder.uid,
                    name: folder.name,
                },
                children,
            })
        };
        tokio::select! {
            biased;
            _ = account.cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(90), work) =>
                result.map_err(|_| Error::message("Proton took too long to respond. Try again."))?,
        }
    }

    pub async fn scan_drive_folder(&self, folder: NodeUid) -> Result<SourceId> {
        let _operations = self.operations.read().await;
        let _scan = self.drive_sync.lock().await;
        let account = self.auth.account().await?;
        let root = tokio::select! {
            biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            result = account.drive.get_node(&folder) => result
                .map_err(drive_error("Couldn’t open this Drive folder."))?
                .ok_or(proton_gallery_rs::Error::NotFound)?,
        };
        Ok(proton_gallery_drive_rs::galleries::scan_gallery(
            &self.catalog,
            account.drive,
            account.id,
            root,
            true,
            &account.cancel,
            |_, _| {},
        )
        .await?)
    }

    pub async fn add_drive_folder(&self, folder: NodeUid) -> Result<SourceId> {
        let generation = self.generation();
        let gallery = self.scan_drive_folder(folder).await?;
        let _operations = self.operations.read().await;
        let account = self
            .auth
            .account_id()
            .ok_or(proton_gallery_rs::Error::NotFound)?;
        self.ensure_current(generation)?;
        self.catalog
            .set_gallery_configuration_membership(account, gallery.clone())
            .await?;
        Ok(gallery)
    }

    /// Remove the gallery from the catalog. Its Proton folder stays in Drive.
    pub async fn forget_drive_gallery(&self, gallery: SourceId) -> Result<()> {
        let _operations = self.operations.read().await;
        let _scan = self.drive_sync.lock().await;
        let account = self
            .auth
            .account_id()
            .ok_or(proton_gallery_rs::Error::NotFound)?;
        Ok(self.catalog.remove_gallery(gallery, account).await?)
    }
}
