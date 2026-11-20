//! Proton Photos albums, brought in line with Proton after each Photos sync.
//! Album browsing is shared; create/rename/membership editing remains in the mobile adapter.
use super::*;
use proton_drive_rs::ProtonPhotosClient;
use proton_gallery_rs::model::{
    AccountId, Asset, AssetKey, NodeId, SourceId, SourceSpec, VolumeId,
};
use proton_sdk::ids::NodeUid;
use sha2::{Digest, Sha256};

/// The catalog source of a Proton album.
pub fn album_spec(account: &AccountId, uid: &NodeUid, name: &str) -> Result<SourceSpec> {
    let identity = format!(
        "{}\n{}\n{}",
        account.as_str(),
        uid.volume_id.as_str(),
        uid.link_id.as_str()
    );
    Ok(SourceSpec {
        id: SourceId::new(format!(
            "album-{}",
            hex::encode(Sha256::digest(identity.as_bytes()))
        ))?,
        account: account.clone(),
        volume: VolumeId::new(uid.volume_id.as_str())?,
        root: NodeId::new(uid.link_id.as_str())?,
        label: name.to_owned(),
    })
}

pub fn album_uid(spec: &SourceSpec) -> NodeUid {
    NodeUid::new(spec.volume.as_str().into(), spec.root.as_str().into())
}

/// Log a failed Proton call and describe it for the user.
pub fn album_failure(what: &'static str) -> impl Fn(proton_sdk::error::ProtonError) -> Error {
    move |e| {
        tracing::warn!(
            "{what} failed: {}",
            proton_gallery_drive_rs::auth::cause_chain(&e)
        );
        Error::message(format!(
            "Couldn’t {what}. Check your connection and try again."
        ))
    }
}

impl App {
    /// Visible albums of the active account; hidden mobile albums stay hidden.
    pub async fn photo_albums(&self) -> Result<Vec<proton_gallery_rs::library::Album>> {
        self.photo_albums_with_hidden(false).await
    }

    pub async fn photo_albums_with_hidden(
        &self,
        unlocked: bool,
    ) -> Result<Vec<proton_gallery_rs::library::Album>> {
        let generation = self.generation();
        let _operations = self.operations.read().await;
        let Some(account) = self.auth.account_id() else {
            return Ok(vec![]);
        };
        self.require_gallery_visibility().await?;
        let albums = self.catalog.photo_albums(account).await?;
        self.ensure_current(generation)?;
        Ok(albums
            .into_iter()
            .filter(|album| unlocked || !album.settings.hidden)
            .collect())
    }

    /// Resolve one album at a time so a partial pass can resume from disk.
    pub(crate) async fn sync_photo_album(
        &self,
        photos: &ProtonPhotosClient,
        account: &AccountId,
        uid: &NodeUid,
    ) -> Result<()> {
        let node = photos
            .enumerate_nodes(std::slice::from_ref(uid))
            .await
            .map_err(crate::sync::photos_error("read an album"))?
            .into_iter()
            .find(|node| &node.uid == uid)
            .ok_or_else(|| Error::message("Some album details are still unavailable."))?;
        let spec = album_spec(account, uid, &node.name)?;
        if node.trashed {
            self.catalog.forget_photo_album(spec.id).await?;
        } else {
            let items = photos
                .enumerate_album(uid)
                .await
                .map_err(crate::sync::photos_error("read an album"))?;
            let assets = self.album_assets(photos, account, items).await?;
            self.catalog.sync_photo_album(spec, assets).await?;
        }
        Ok(())
    }

    /// Catalog records for album photos; photos the timeline does not hold
    /// (kept only in the album) are read from Proton.
    pub(crate) async fn album_assets(
        &self,
        photos: &ProtonPhotosClient,
        account: &AccountId,
        items: Vec<proton_drive_rs::AlbumItem>,
    ) -> Result<Vec<Asset>> {
        let keys = items
            .iter()
            .map(|item| {
                Ok(AssetKey {
                    account: account.clone(),
                    volume: VolumeId::new(item.uid.volume_id.as_str())?,
                    node: NodeId::new(item.uid.link_id.as_str())?,
                })
            })
            .collect::<Result<Vec<_>, proton_gallery_rs::Error>>()?;
        let known = self.catalog.assets_by_key(keys).await?;
        let mut assets = Vec::with_capacity(items.len());
        let mut missing = Vec::new();
        for (item, asset) in items.iter().zip(known) {
            match asset {
                Some(asset) => assets.push(asset),
                None => missing.push(item),
            }
        }
        for batch in missing.chunks(100) {
            let uids: Vec<NodeUid> = batch.iter().map(|item| item.uid.clone()).collect();
            let nodes = photos
                .enumerate_nodes(&uids)
                .await
                .map_err(crate::sync::photos_error("read an album"))?;
            if nodes.len() != uids.len() {
                return Err(Error::message("Some album photos are still unavailable."));
            }
            for node in nodes {
                let Some(item) = batch.iter().find(|item| item.uid == node.uid) else {
                    continue;
                };
                if let Some(asset) =
                    proton_gallery_drive_rs::photos::photo_asset(account, node, item.capture_time)?
                {
                    assets.push(asset);
                }
            }
        }
        Ok(assets)
    }
}
