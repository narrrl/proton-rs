//! Proton Photos synchronization into the local catalog.
use super::*;
use proton_drive_rs::{DriveEvent, DriveEventScopeId, Node, ProtonPhotosClient};
use proton_gallery_drive_rs::auth::Account;
use proton_gallery_rs::CancellationToken;
use proton_gallery_rs::model::{NodeId, ScanCheckpoint, SourceSpec};
use proton_gallery_rs::photos_sync::{
    PendingPhoto, PhotoChange, PhotoResolution, PhotosCheckpoint,
};
use proton_sdk::ids::{DriveEventId, NodeUid};
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const METADATA_BATCH: usize = 100;
const BATCHES_PER_CHECK: usize = 4;
const ALBUMS_PER_CHECK: usize = 4;

#[derive(Clone, Copy, Debug, Default)]
pub struct PhotosSyncStatus {
    pub revision: u64,
    /// Seconds since the Unix epoch.
    pub last_success: Option<i64>,
    /// More work is ready for another bounded check.
    pub more: bool,
    /// Metadata or albums need to be retried with backoff.
    pub retry: bool,
}

impl From<PhotosCheckpoint> for PhotosSyncStatus {
    fn from(checkpoint: PhotosCheckpoint) -> Self {
        Self {
            revision: checkpoint.revision,
            last_success: checkpoint.last_success,
            ..Self::default()
        }
    }
}
use proton_gallery_rs::source::MediaSource;

/// Progress of a running sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncProgress {
    /// Media indexed so far.
    pub indexed: u64,
}

impl App {
    /// Index `source` into the catalog under `generation`. A sign-out while
    /// it runs cancels it; a session change before it finishes makes its
    /// result stale.
    pub async fn index_source(
        &self,
        source: &dyn MediaSource,
        generation: Generation,
        cancel: &CancellationToken,
        progress: impl Fn(SyncProgress),
    ) -> Result<ScanCheckpoint> {
        let _operations = self.operations.read().await;
        self.scan_source(source, generation, cancel, progress).await
    }

    async fn scan_source(
        &self,
        source: &dyn MediaSource,
        generation: Generation,
        cancel: &CancellationToken,
        progress: impl Fn(SyncProgress),
    ) -> Result<ScanCheckpoint> {
        self.ensure_current(generation)?;
        let checkpoint =
            proton_gallery_rs::indexer::scan(&self.catalog, source, true, cancel, |p| {
                progress(SyncProgress { indexed: p.indexed })
            })
            .await?;
        self.ensure_current(generation)?;
        Ok(checkpoint)
    }

    pub async fn photos_sync_status(&self) -> Result<PhotosSyncStatus> {
        let Some(account) = self.auth.account_id() else {
            return Ok(PhotosSyncStatus::default());
        };
        Ok(self
            .catalog
            .account_photos_sync(account)
            .await?
            .map(|(_, checkpoint)| checkpoint.into())
            .unwrap_or_default())
    }

    pub async fn sync_photos(&self, progress: impl Fn(SyncProgress)) -> Result<u64> {
        loop {
            let status = self.sync_photos_batch(false, &progress).await?;
            if !status.more {
                return Ok(self.indexed.load(Ordering::Relaxed));
            }
        }
    }

    pub async fn sync_photos_batch(
        &self,
        repair: bool,
        progress: impl Fn(SyncProgress),
    ) -> Result<PhotosSyncStatus> {
        let generation = self.generation();
        let _operations = self.operations.read().await;
        let _guard = self.photos_sync.lock().await;
        self.ensure_current(generation)?;
        let account = self.auth.account().await?;
        let mut queued = false;
        if repair
            && let Some((source, _)) = self.catalog.account_photos_sync(account.id.clone()).await?
        {
            self.catalog.queue_photos_repair(source).await?;
            queued = true;
        }
        self.indexed.store(0, Ordering::Relaxed);
        let result = tokio::select! { biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(45), self.check_photos(&account, generation, repair && !queued, progress)) =>
                result.map_err(|_| Error::message("Proton took too long to check Photos. Your cached library is available."))?,
        };
        self.ensure_current(generation)?;
        result
    }

    async fn check_photos(
        &self,
        account: &Account,
        generation: Generation,
        repair: bool,
        progress: impl Fn(SyncProgress),
    ) -> Result<PhotosSyncStatus> {
        let photos = ProtonPhotosClient::from_drive_client(account.drive.clone());
        let cached = self
            .photos
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|source| source.spec().account == account.id)
            .cloned();
        let source = match cached {
            Some(source) => source,
            None => {
                let Some(root) = photos
                    .get_photos_root()
                    .await
                    .map_err(photos_error("open Proton Photos"))?
                else {
                    return Ok(PhotosSyncStatus::default());
                };
                let source = Arc::new(proton_gallery_drive_rs::photos::ProtonPhotosSource::new(
                    photos.clone(),
                    account.id.clone(),
                    &root,
                )?);
                let mut cache = self.photos.write().unwrap_or_else(|e| e.into_inner());
                self.ensure_current(generation)?;
                *cache = Some(source.clone());
                source
            }
        };
        let source = source.spec();
        let mut checkpoint = self.catalog.open_photos_sync(source.clone()).await?;
        if repair {
            self.catalog.queue_photos_repair(source.id.clone()).await?;
            checkpoint = self.catalog.photos_checkpoint(source.id.clone()).await?;
        }
        checkpoint = self
            .poll_photos(account, &photos, source, checkpoint)
            .await?;
        if checkpoint.listing != 0 {
            let timeline = photos
                .enumerate_timeline()
                .await
                .map_err(photos_error("list Proton Photos"))?;
            let listed = timeline
                .into_iter()
                .map(|item| {
                    if item.uid.volume_id.as_str() != source.volume.as_str() {
                        return Err(proton_gallery_rs::Error::Invalid(
                            "timeline item outside the Photos volume".into(),
                        ));
                    }
                    Ok((NodeId::new(item.uid.link_id.as_str())?, item.capture_time))
                })
                .collect::<proton_gallery_rs::Result<_>>()?;
            self.catalog
                .commit_photos_listing(source.clone(), checkpoint, listed)
                .await?;
            checkpoint = self.catalog.photos_checkpoint(source.id.clone()).await?;
            checkpoint = self
                .poll_photos(account, &photos, source, checkpoint)
                .await?;
            if checkpoint.listing != 0 {
                return Ok(PhotosSyncStatus {
                    more: true,
                    ..checkpoint.into()
                });
            }
        }

        let mut work = self
            .catalog
            .pending_photos(
                source.id.clone(),
                (METADATA_BATCH * BATCHES_PER_CHECK + 1) as u32,
            )
            .await?;
        let more = work
            .get(METADATA_BATCH * BATCHES_PER_CHECK)
            .is_some_and(|pending| pending.attempts == 0);
        work.truncate(METADATA_BATCH * BATCHES_PER_CHECK);
        let mut indexed = 0;
        while !work.is_empty() {
            let batch: Vec<_> = work.drain(..work.len().min(METADATA_BATCH)).collect();
            let uids: Vec<_> = batch
                .iter()
                .map(|pending| photos_uid(source, &pending.node))
                .collect();
            let mut nodes: HashMap<_, _> = photos
                .enumerate_nodes(&uids)
                .await
                .map_err(photos_error("read photo metadata"))?
                .into_iter()
                .map(|node| (node.uid.clone(), node))
                .collect();
            let resolutions = batch
                .into_iter()
                .zip(uids)
                .map(|(pending, uid)| {
                    let result = resolve_photo(source, &pending, nodes.remove(&uid))?;
                    Ok((pending, result))
                })
                .collect::<Result<_>>()?;
            indexed += self
                .catalog
                .commit_photos_metadata(source.clone(), resolutions)
                .await?;
            self.indexed.store(indexed, Ordering::Relaxed);
            progress(SyncProgress { indexed });
        }
        if more {
            return Ok(PhotosSyncStatus {
                more,
                ..self.photos_sync_status().await?
            });
        }

        let mut album_more = false;
        if checkpoint.albums_pending {
            // Metadata and deletions are already committed. Each album is
            // acknowledged on its own and a failed one goes to the end.
            match self
                .check_photos_albums(&photos, source, checkpoint.album_listing)
                .await
            {
                Ok(more) => album_more = more,
                Err(Error::SessionExpired) => return Err(Error::SessionExpired),
                Err(error) => tracing::warn!("Photos album reconciliation failed: {error}"),
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.catalog
            .finish_photos_sync(source.id.clone(), now)
            .await?;
        let checkpoint = self.catalog.photos_checkpoint(source.id.clone()).await?;
        let pending = !self
            .catalog
            .pending_photos(source.id.clone(), 1)
            .await?
            .is_empty();
        Ok(PhotosSyncStatus {
            more: album_more,
            retry: pending || checkpoint.albums_pending,
            ..checkpoint.into()
        })
    }

    async fn check_photos_albums(
        &self,
        photos: &ProtonPhotosClient,
        source: &SourceSpec,
        list: bool,
    ) -> Result<bool> {
        if list {
            let uids =
                tokio::time::timeout(Duration::from_secs(15), photos.enumerate_album_node_uids())
                    .await
                    .map_err(|_| Error::message("Listing Photos albums timed out."))?
                    .map_err(photos_error("list Photos albums"))?;
            let albums = uids
                .iter()
                .map(|uid| crate::albums::album_spec(&source.account, uid, ""))
                .collect::<Result<_>>()?;
            self.catalog
                .commit_photos_album_listing(source.clone(), albums)
                .await?;
        }
        let work = self
            .catalog
            .pending_photos_albums(source.id.clone(), ALBUMS_PER_CHECK as u32 + 1)
            .await?;
        let more = work
            .get(ALBUMS_PER_CHECK)
            .is_some_and(|(_, attempts)| *attempts == 0);
        for (node, _) in work.into_iter().take(ALBUMS_PER_CHECK) {
            let uid = photos_uid(source, &node);
            let result = tokio::time::timeout(
                Duration::from_secs(6),
                self.sync_photo_album(photos, &source.account, &uid),
            )
            .await;
            if matches!(result, Ok(Err(Error::SessionExpired))) {
                return Err(Error::SessionExpired);
            }
            let done = matches!(result, Ok(Ok(())));
            if !done {
                tracing::warn!("A Photos album will be retried");
            }
            self.catalog
                .finish_photos_album(source.id.clone(), node, done)
                .await?;
        }
        Ok(more)
    }

    async fn poll_photos(
        &self,
        account: &Account,
        photos: &ProtonPhotosClient,
        source: &SourceSpec,
        checkpoint: PhotosCheckpoint,
    ) -> Result<PhotosCheckpoint> {
        let scope = DriveEventScopeId::new(source.volume.as_str().into());
        let cursor = checkpoint.cursor.clone().map(DriveEventId::from);
        let events = photos
            .enumerate_events(&scope, cursor.as_ref())
            .await
            .map_err(photos_error("check for Photos changes"))?;
        let next = events
            .last()
            .map(|event| event.id().as_str().to_owned())
            .or(checkpoint.cursor.clone())
            .ok_or_else(|| Error::message("Proton did not supply a Photos checkpoint."))?;
        if events.is_empty() && !checkpoint.replay {
            return Ok(checkpoint);
        }

        let mut gap = false;
        let mut changes = Vec::new();
        for event in &events {
            account
                .drive
                .invalidate_caches_for_event(event)
                .await
                .map_err(photos_error("invalidate changed photo metadata"))?;
            let (uid, removed) = match event {
                DriveEvent::NodeDeleted { node_uid, .. } => (node_uid, true),
                DriveEvent::NodeUpdated {
                    node_uid,
                    is_trashed,
                    ..
                } => (node_uid, *is_trashed),
                DriveEvent::ContinuityLost { .. } | DriveEvent::ScopeAccessLost { .. } => {
                    gap = true;
                    continue;
                }
                _ => continue,
            };
            if uid.volume_id.as_str() == source.volume.as_str() {
                let node = NodeId::new(uid.link_id.as_str())?;
                changes.push(if removed {
                    PhotoChange::Removed(node)
                } else {
                    PhotoChange::Updated(node)
                });
            }
        }
        self.catalog
            .commit_photos_events(source.clone(), checkpoint.cursor, next, changes, gap)
            .await?;
        Ok(self.catalog.photos_checkpoint(source.id.clone()).await?)
    }
}

fn photos_uid(source: &SourceSpec, node: &NodeId) -> NodeUid {
    NodeUid::new(source.volume.as_str().into(), node.as_str().into())
}

fn resolve_photo(
    source: &SourceSpec,
    pending: &PendingPhoto,
    node: Option<Node>,
) -> Result<PhotoResolution> {
    let Some(node) = node else {
        return Ok(PhotoResolution::Unresolved);
    };
    if node.trashed {
        return Ok(PhotoResolution::Removed);
    }
    let timeline = pending.capture_time.is_some()
        || node
            .parent_uid
            .as_ref()
            .is_some_and(|parent| *parent == photos_uid(source, &source.root));
    let capture_time = pending
        .capture_time
        .or_else(|| node.photo.as_ref().map(|photo| photo.capture_time))
        .unwrap_or(node.creation_time);
    Ok(
        match proton_gallery_drive_rs::photos::photo_asset(&source.account, node, capture_time)? {
            Some(asset) => PhotoResolution::Asset {
                asset: Box::new(asset),
                timeline,
            },
            None => PhotoResolution::Ignore,
        },
    )
}

pub(crate) fn photos_error(what: &'static str) -> impl Fn(proton_sdk::ProtonError) -> Error {
    move |error| {
        use proton_sdk::api::ResponseCode;
        if matches!(&error, proton_sdk::ProtonError::Api(api)
            if api.is_invalid_refresh_token() || matches!(api.code, ResponseCode::AccountDeleted | ResponseCode::AccountDisabled))
        {
            return Error::SessionExpired;
        }
        tracing::warn!(
            "{what} failed: {}",
            proton_gallery_drive_rs::auth::cause_chain(&error)
        );
        Error::message(format!(
            "Couldn’t {what}. Your cached library is available."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use proton_gallery_rs::synthetic::SyntheticSource;
    use std::sync::Mutex;
    use std::time::Duration;

    #[tokio::test]
    async fn a_failed_sync_can_be_retried_to_completion() {
        let (_dir, app) = app(Arc::default()).await;
        let cancel = CancellationToken::new();
        let generation = app.generation();
        let failing = SyntheticSource::new(600).unwrap().fail_at(250);
        let error = app
            .index_source(&failing, generation, &cancel, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Message(_)));
        let source = SyntheticSource::new(600).unwrap();
        let seen = Mutex::new(Vec::new());
        let done = app
            .index_source(&source, generation, &cancel, |p| {
                seen.lock().unwrap().push(p.indexed)
            })
            .await
            .unwrap();
        assert!(done.complete);
        assert_eq!(
            app.catalog
                .source_count(source.spec().id.clone())
                .await
                .unwrap(),
            600
        );
        assert!(
            seen.lock()
                .unwrap()
                .last()
                .is_some_and(|&n| n == done.indexed)
        );
    }

    #[tokio::test]
    async fn cancelling_stops_a_sync() {
        let (_dir, app) = app(Arc::default()).await;
        let cancel = CancellationToken::new();
        let source = SyntheticSource::new(10_000)
            .unwrap()
            .with_delay(Duration::from_millis(20));
        let generation = app.generation();
        let run = app.index_source(&source, generation, &cancel, |_| {});
        let stop = async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(run, stop);
        assert_eq!(result.unwrap_err(), Error::Cancelled);
    }

    #[tokio::test]
    async fn a_sync_that_outlives_its_account_returns_no_result() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault).await;
        app.restore_session().await.unwrap();
        let generation = app.generation();
        let cancel = CancellationToken::new();
        let source = SyntheticSource::new(5_000)
            .unwrap()
            .with_delay(Duration::from_millis(10));
        let run = app.index_source(&source, generation, &cancel, |_| {});
        let switch = async {
            tokio::time::sleep(Duration::from_millis(15)).await;
            app.advance();
        };
        let (result, ()) = tokio::join!(run, switch);
        assert_eq!(result.unwrap_err(), Error::Stale);
        // Work started after the change is current again.
        assert!(
            app.index_source(&source, app.generation(), &cancel, |_| {})
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn syncing_photos_needs_an_account() {
        let (_dir, app) = app(Arc::default()).await;
        let error = app.sync_photos(|_| {}).await.unwrap_err();
        assert!(matches!(error, Error::Message(text) if text.contains("Sign in")));
    }
}
