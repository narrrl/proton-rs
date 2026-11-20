//! Proton favorite mutations shared by desktop and the Android adapter.
use super::*;
use proton_gallery_rs::model::AssetKey;

pub const MAX_FAVORITE_ITEMS: usize = 5_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FavoriteOutcome {
    pub changed: u64,
    pub failed: u64,
}

impl App {
    pub async fn set_favorite(
        &self,
        keys: Vec<AssetKey>,
        favorite: bool,
    ) -> Result<FavoriteOutcome> {
        if keys.len() > MAX_FAVORITE_ITEMS {
            return Err(Error::message("Too many items to change at once."));
        }
        let generation = self.generation();
        let _operations = self.operations.read().await;
        // Sync snapshots include favorites, so finish indexing before updating tags.
        let _sync = self.photos_sync.lock().await;
        self.ensure_current(generation)?;
        let account = self.auth.account().await?;
        let photos_volume = self
            .catalog
            .account_sources(
                proton_gallery_rs::library::SourceKind::Photos,
                account.id.clone(),
            )
            .await?
            .first()
            .map(|source| source.volume.clone());
        let mut accepted = Vec::new();
        let mut failed = 0;
        // Reject an account mismatch before issuing any network mutations.
        for key in keys {
            self.owns(&account.id, &key)?;
            if Some(&key.volume) == photos_volume.as_ref() {
                accepted.push(key);
            } else {
                failed += 1;
            }
        }
        let photos = proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone());
        let mut changed = Vec::new();
        for batch in accepted.chunks(50) {
            let updates: Vec<_> = batch
                .iter()
                .map(|key| proton_drive_rs::PhotoTagsUpdate {
                    node_uid: proton_gallery_drive_rs::node_uid(key),
                    tags_to_add: if favorite {
                        vec![proton_drive_rs::PhotoTag::Favorite]
                    } else {
                        vec![]
                    },
                    tags_to_remove: if favorite {
                        vec![]
                    } else {
                        vec![proton_drive_rs::PhotoTag::Favorite]
                    },
                })
                .collect();
            let outcomes = tokio::select! { biased;
                _ = account.cancel.cancelled() => return Err(Error::Cancelled),
                result = photos.update_photos(&updates) => result.map_err(crate::albums::album_failure("change the favorites"))?,
            };
            for (key, update) in batch.iter().zip(&updates) {
                if outcomes
                    .iter()
                    .any(|(uid, result)| uid == &update.node_uid && result.is_ok())
                {
                    changed.push(key.clone());
                } else {
                    failed += 1;
                }
            }
        }
        self.ensure_current(generation)?;
        if account.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let count = changed.len() as u64;
        self.catalog.set_favorite(changed, favorite).await?;
        Ok(FavoriteOutcome {
            changed: count,
            failed,
        })
    }
}
