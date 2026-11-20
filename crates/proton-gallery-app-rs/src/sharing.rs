//! Public viewer links for owned Drive gallery folders.
use super::*;
pub use proton_drive_rs::PublicLink;
use proton_drive_rs::{MemberRole, Node};
use proton_gallery_rs::model::{AccountId, SourceId};
use proton_sdk::ids::NodeUid;

pub enum PublicLinkAction {
    Read,
    Create {
        password: zeroize::Zeroizing<String>,
        expires: Option<i64>,
    },
    Revoke,
}

impl App {
    /// Read or manage a public Drive folder link. Photos albums use a different
    /// sharing model and are intentionally outside this interface.
    pub async fn drive_public_link(
        &self,
        gallery: String,
        album: String,
        action: PublicLinkAction,
    ) -> Result<Option<PublicLink>> {
        let generation = self.generation();
        if matches!(action, PublicLinkAction::Create { .. }) {
            self.refresh_gallery_configuration(true).await?;
        }
        let _operations = self.operations.read().await;
        self.ensure_current(generation)?;
        self.require_gallery_visibility().await?;
        let account = self.auth.account().await?;
        let source = self.checked_album(gallery, album).await?;
        let folder = self.owned_drive_folder(source, account.id.clone()).await?;
        let uid = super::albums::album_uid(&folder);
        let task = async {
            let hierarchy = account
                .drive
                .get_node_hierarchy(&uid)
                .await
                .map_err(super::albums::album_failure("verify this Drive folder"))?
                .ok_or(proton_gallery_rs::Error::NotFound)?;
            let my_files = account
                .drive
                .get_my_files_folder()
                .await
                .map_err(super::albums::album_failure("verify the folder owner"))?;
            if hierarchy.len() < 2
                || hierarchy
                    .first()
                    .is_none_or(|node| node.uid != my_files.uid)
                || hierarchy
                    .iter()
                    .any(|node| node.trashed || node.membership.is_some())
            {
                return Err(Error::message(
                    "Public links require an owned folder under My files.",
                ));
            }
            match action {
                PublicLinkAction::Read => account
                    .drive
                    .get_public_link(&uid)
                    .await
                    .map_err(super::albums::album_failure("read the public link")),
                PublicLinkAction::Revoke => {
                    if let Some(link) = account
                        .drive
                        .get_public_link(&uid)
                        .await
                        .map_err(super::albums::album_failure("read the public link"))?
                    {
                        account
                            .drive
                            .remove_public_link(&link)
                            .await
                            .map_err(super::albums::album_failure("revoke the public link"))?;
                    }
                    Ok(None)
                }
                PublicLinkAction::Create { password, expires } => {
                    self.require_shareable_folder(&account, &uid, &hierarchy)
                        .await?;
                    if let Some(link) = account
                        .drive
                        .get_public_link(&uid)
                        .await
                        .map_err(super::albums::album_failure("read the public link"))?
                    {
                        return Ok(Some(link));
                    }
                    account
                        .drive
                        .create_public_link(
                            &uid,
                            MemberRole::Viewer,
                            (!password.is_empty()).then_some(password.as_str()),
                            expires,
                        )
                        .await
                        .map(Some)
                        .map_err(super::albums::album_failure("create the public link"))
                }
            }
        };
        tokio::select! {
            biased;
            _ = account.cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(90), task) =>
                result.map_err(|_| Error::message("Proton took too long to update this public link."))?,
        }
    }

    async fn owned_drive_folder(
        &self,
        source: SourceId,
        account: AccountId,
    ) -> Result<proton_gallery_rs::model::SourceSpec> {
        self.catalog
            .account_source(SourceKind::Drive, account, source)
            .await?
            .ok_or_else(|| proton_gallery_rs::Error::NotFound.into())
    }

    async fn require_shareable_folder(
        &self,
        account: &proton_gallery_drive_rs::auth::Account,
        target: &NodeUid,
        hierarchy: &[Node],
    ) -> Result<()> {
        const PRIVATE: &str = "This folder contains hidden media or is inside a hidden gallery.";
        let hidden = self
            .catalog
            .hidden_configuration_albums(account.id.clone())
            .await?;
        for folder in hidden.into_iter().filter(|album| !album.photos_album) {
            let uid = NodeUid::new(folder.volume.as_str().into(), folder.node.as_str().into());
            if uid.volume_id != target.volume_id {
                continue;
            }
            if hierarchy.iter().any(|node| node.uid == uid) {
                return Err(Error::message(PRIVATE));
            }
            let ancestry = account
                .drive
                .get_node_hierarchy(&uid)
                .await
                .map_err(super::albums::album_failure("check hidden Drive folders"))?;
            if ancestry.is_some_and(|nodes| nodes.iter().any(|node| &node.uid == target)) {
                return Err(Error::message(PRIVATE));
            }
        }
        Ok(())
    }
}
