//! Versioned, immutable settings records stored in the account's Gallery device.
use super::*;
use proton_drive_rs::{Device, DeviceType, DriveEvent, ProtonDriveClient, ProtonPhotosClient};
use proton_gallery_rs::configuration::{RECORD_BYTES, Record};
use proton_gallery_rs::model::PageSize;
use proton_sdk::ids::{DriveEventId, NodeUid};
use std::collections::{BTreeMap, btree_map::Entry};

const ROOT_NAME: &str = "Proton Gallery settings";
const RECORD_FOLDER: &str = "Settings v1";
const MARKER_NAME: &str = "format.json";
const MARKER: &[u8] = br#"{"format":"proton-gallery-settings","version":1}"#;

fn request_error(what: &'static str) -> impl Fn(proton_sdk::error::ProtonError) -> Error {
    move |error| {
        tracing::warn!(
            "{what} {}",
            proton_gallery_drive_rs::auth::cause_chain(&error)
        );
        Error::message(what)
    }
}

/// A matched device root is only a candidate until its format marker is read.
struct ConfigStore {
    drive: ProtonDriveClient,
    root: NodeUid,
}

impl ConfigStore {
    async fn devices(drive: &ProtonDriveClient, name: &str) -> Result<Vec<Device>> {
        let listed = drive
            .enumerate_devices()
            .await
            .map_err(request_error("Couldn’t find Proton Gallery settings."))?;
        Ok(listed
            .into_iter()
            .filter(|device| {
                device.device_type == DeviceType::Linux && device.name.as_deref().ok() == Some(name)
            })
            .collect())
    }

    async fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() > RECORD_BYTES {
            return Err(Error::message("The settings record is too large."));
        }
        self.drive
            .upload_file(&self.root, name, "application/json", bytes)
            .await
            .map_err(request_error("Couldn’t upload a Gallery settings record."))?;
        Ok(())
    }

    async fn read(&self, uid: &NodeUid) -> Result<Vec<u8>> {
        struct Bounded(Vec<u8>);
        impl std::io::Write for Bounded {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len() + bytes.len() > RECORD_BYTES {
                    return Err(std::io::Error::other(
                        "Gallery settings record exceeds size limit",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut bytes = Bounded(Vec::new());
        self.drive
            .download_file_to(uid, &mut bytes)
            .await
            .map_err(request_error("Couldn’t verify a Gallery settings record."))?;
        Ok(bytes.0)
    }

    async fn list(
        drive: &ProtonDriveClient,
        parent: &NodeUid,
    ) -> Result<Vec<proton_drive_rs::Node>> {
        let ids = drive
            .enumerate_folder_children_node_uids(parent)
            .await
            .map_err(request_error("Couldn’t list Gallery settings."))?;
        let mut nodes = Vec::new();
        for batch in ids.chunks(PageSize::MAX) {
            let returned = drive.enumerate_nodes(batch).await.map_err(request_error(
                "Couldn’t decrypt Gallery settings file names.",
            ))?;
            if returned.len() != batch.len() {
                return Err(Error::message(
                    "Some Gallery settings files couldn’t be read.",
                ));
            }
            nodes.extend(returned.into_iter().filter(|node| !node.trashed));
        }
        Ok(nodes)
    }

    async fn open(drive: &ProtonDriveClient, device: Device, writable: bool) -> Result<Self> {
        let children = Self::list(drive, &device.root_folder_uid).await?;
        let root = if let Some(folder) = children
            .into_iter()
            .find(|node| node.name == RECORD_FOLDER && node.is_folder())
        {
            folder.uid
        } else if writable {
            drive
                .create_folder_path(&device.root_folder_uid, RECORD_FOLDER)
                .await
                .map_err(request_error(
                    "Couldn’t create a folder for Gallery settings.",
                ))?
        } else {
            return Err(Error::message(
                "The Gallery settings device is still being set up.",
            ));
        };
        let store = Self {
            drive: drive.clone(),
            root,
        };
        let files = Self::list(drive, &store.root).await?;
        if let Some(marker) = files
            .iter()
            .find(|node| node.name == MARKER_NAME && node.is_file())
        {
            if store.read(&marker.uid).await? != MARKER {
                return Err(Error::message(
                    "The Gallery settings format is unsupported.",
                ));
            }
        } else if files.is_empty() && writable {
            store.write(MARKER_NAME, MARKER).await?;
        } else {
            return Err(Error::message(
                "The Gallery settings format marker is missing.",
            ));
        }
        Ok(store)
    }

    async fn read_records(&self, known: &BTreeMap<String, Record>) -> Result<Vec<Record>> {
        let mut records = Vec::new();
        for node in Self::list(&self.drive, &self.root).await? {
            if node.name == MARKER_NAME {
                continue;
            }
            let Some(id) = node
                .name
                .strip_suffix(".json")
                .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .filter(|_| node.is_file())
            else {
                return Err(Error::message(
                    "Unexpected file in the Gallery settings folder.",
                ));
            };
            if let Some(record) = known.get(id).filter(|record| !record.pending) {
                records.push(record.clone());
                continue;
            }
            let document = String::from_utf8(self.read(&node.uid).await?)
                .map_err(|_| Error::message("A Gallery settings record has invalid text."))?;
            let record = Record {
                id: id.into(),
                document,
                pending: false,
            };
            record.decode()?;
            records.push(record);
        }
        Ok(records)
    }
}

impl App {
    /// Background settings poll. An idle cursor does not enumerate devices,
    /// settings files, or albums. These cursors track settings only; Photos
    /// and Drive media scanning do not consume them.
    pub async fn poll_gallery_configuration(&self, import_current: bool) -> Result<Option<bool>> {
        let _poll = self.configuration_poll.lock().await;
        let generation = self.generation();
        let (account, cursors, changed) = {
            let _operations = self.operations.read().await;
            let account = self.auth.account().await?;
            let saved = self
                .catalog
                .gallery_event_cursors(account.id.clone())
                .await?;
            let pending = self
                .catalog
                .configuration_pending(account.id.clone())
                .await?;
            let poll = async {
                let photos = ProtonPhotosClient::from_drive_client(account.drive.clone());
                let drive_root = account
                    .drive
                    .get_my_files_folder()
                    .await
                    .map_err(request_error("Couldn’t find the Drive change feed."))?;
                let mut scopes = vec![drive_root.tree_event_scope_id()];
                if let Some(root) = photos
                    .get_photos_root()
                    .await
                    .map_err(request_error("Couldn’t find the Photos change feed."))?
                {
                    scopes.push(root.tree_event_scope_id());
                }
                let mut next = BTreeMap::new();
                let mut changed = pending;
                for scope in scopes {
                    let volume = scope.volume_id().as_str();
                    let cursor = saved.get(volume).cloned().map(DriveEventId::from);
                    changed |= cursor.is_none();
                    let events = account
                        .drive
                        .enumerate_events(&scope, cursor.as_ref())
                        .await
                        .map_err(request_error("Couldn’t check for Gallery updates."))?;
                    for event in &events {
                        if !matches!(event, DriveEvent::CursorAdvanced { .. }) {
                            changed = true;
                            account
                                .drive
                                .invalidate_caches_for_event(event)
                                .await
                                .map_err(request_error("Couldn’t check newly changed settings."))?;
                        }
                    }
                    if let Some(last) = events.last() {
                        next.insert(volume.to_owned(), last.id().as_str().to_owned());
                    }
                }
                Ok::<_, Error>((next, changed))
            };
            let (cursors, changed) = tokio::select! {
                biased;
                _ = account.cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::time::timeout(std::time::Duration::from_secs(30), poll) =>
                    result.map_err(|_| Error::message("Proton took too long to check for changes."))??,
            };
            (account.id, cursors, changed)
        };
        self.ensure_current(generation)?;
        let result = if changed {
            Some(self.refresh_gallery_configuration(import_current).await?)
        } else {
            None
        };
        if !cursors.is_empty() {
            let _operations = self.operations.read().await;
            self.ensure_current(generation)?;
            self.catalog
                .save_gallery_event_cursors(account, cursors)
                .await?;
        }
        Ok(result)
    }

    pub async fn gallery_visibility_cached(&self) -> Result<bool> {
        let Some(account) = self.auth.account_id() else {
            return Ok(true);
        };
        Ok(self
            .catalog
            .has_verified_gallery_visibility(account)
            .await?)
    }

    pub(crate) async fn require_gallery_visibility(&self) -> Result<()> {
        if self.gallery_visibility_cached().await? {
            Ok(())
        } else {
            Err(Error::message(
                "Checking hidden albums. Connect to Proton and refresh before opening your library.",
            ))
        }
    }

    /// Refresh visibility and report whether the hidden items or albums changed.
    pub async fn refresh_gallery_configuration(&self, import_current: bool) -> Result<bool> {
        let _sync = self.configuration_sync.lock().await;
        let generation = self.generation();
        let account = self.auth.account().await?;
        let mut before = self.catalog.hidden_assets(account.id.clone()).await?;
        before.sort_unstable();
        let before_albums = self
            .catalog
            .hidden_configuration_albums(account.id.clone())
            .await?;
        let galleries = self
            .synchronize_gallery_configuration(import_current)
            .await?;
        self.ensure_current(generation)?;
        {
            let _operations = self.operations.read().await;
            self.ensure_current(generation)?;
            let photos =
                proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone());
            let hidden = self
                .catalog
                .hidden_configuration_albums(account.id.clone())
                .await?;
            let photos = &photos;
            let protect = async {
                if hidden.iter().any(|album| album.photos_album) {
                    let owned = photos
                        .enumerate_album_node_uids()
                        .await
                        .map_err(super::albums::album_failure("check hidden albums"))?;
                    for album in hidden.into_iter().filter(|album| album.photos_album) {
                        let uid =
                            NodeUid::new(album.volume.as_str().into(), album.node.as_str().into());
                        if !owned.contains(&uid) {
                            continue;
                        }
                        let node = photos
                            .get_node(&uid)
                            .await
                            .map_err(super::albums::album_failure("check a hidden album"))?
                            .ok_or(proton_gallery_rs::Error::NotFound)?;
                        let spec = super::albums::album_spec(&account.id, &uid, &node.name)?;
                        self.catalog.register_source(spec.clone()).await?;
                        self.catalog
                            .set_source_kind(spec.id.clone(), SourceKind::Album)
                            .await?;
                        // Set the hide before writing membership so picker
                        // queries can never see the newly indexed members.
                        self.catalog
                            .merge_configuration(account.id.clone(), vec![])
                            .await?;
                        let items = photos.enumerate_album(&uid).await.map_err(
                            super::albums::album_failure("check a hidden album’s contents"),
                        )?;
                        let expected = items.len();
                        let assets = self.album_assets(photos, &account.id, items).await?;
                        if assets.len() != expected {
                            return Err(Error::message("Some hidden items couldn’t be checked."));
                        }
                        self.catalog.sync_photo_album(spec, assets).await?;
                    }
                }
                self.ensure_current(generation)?;
                self.catalog
                    .verify_gallery_visibility(account.id.clone())
                    .await?;
                Ok(())
            };
            tokio::select! {
                biased;
                _ = account.cancel.cancelled() => return Err(Error::Cancelled),
                result = tokio::time::timeout(std::time::Duration::from_secs(120), protect) =>
                    result.map_err(|_| Error::message("Proton took too long to verify hidden albums."))??,
            }
        }
        if let Err(error) = self
            .reconcile_drive_galleries(generation, account.id.clone(), galleries)
            .await
        {
            self.ensure_current(generation)?;
            tracing::warn!("Couldn’t apply one or more Drive gallery choices: {error}");
        }
        let mut after = self.catalog.hidden_assets(account.id.clone()).await?;
        after.sort_unstable();
        let after_albums = self.catalog.hidden_configuration_albums(account.id).await?;
        self.ensure_current(generation)?;
        Ok(before != after || before_albums != after_albums)
    }

    async fn reconcile_drive_galleries(
        &self,
        generation: Generation,
        account: proton_gallery_rs::model::AccountId,
        galleries: Vec<(proton_gallery_rs::configuration::AlbumIdentity, bool)>,
    ) -> Result<()> {
        let current = self
            .catalog
            .albums_scoped(None, None, Some(account.clone()))
            .await?;
        for (identity, present) in galleries {
            self.ensure_current(generation)?;
            let existing = current.iter().find(|album| {
                album.source.volume == identity.volume && album.source.root == identity.node
            });
            if let Some(album) = existing {
                if !present {
                    self.catalog
                        .apply_gallery_removal(album.gallery.clone(), account.clone())
                        .await?;
                }
            } else if present {
                let uid = NodeUid::new(
                    identity.volume.as_str().into(),
                    identity.node.as_str().into(),
                );
                // Indexing an enrolled folder must not publish a second add.
                if let Err(error) = self.scan_drive_folder(uid).await {
                    self.ensure_current(generation)?;
                    tracing::warn!("Couldn’t restore a synced Drive gallery: {error}");
                }
            }
        }
        self.ensure_current(generation)?;
        self.catalog.merge_configuration(account, vec![]).await?;
        Ok(())
    }

    /// Sync the account's immutable settings journal. Existing settings from
    /// before the journal are imported only on the designated first client.
    async fn synchronize_gallery_configuration(
        &self,
        import_current: bool,
    ) -> Result<Vec<(proton_gallery_rs::configuration::AlbumIdentity, bool)>> {
        let _operations = self.operations.read().await;
        let account = self.auth.account().await?;
        let work = async {
            if import_current {
                self.catalog
                    .import_gallery_configuration(account.id.clone())
                    .await?;
            }
            let local = self
                .catalog
                .configuration_records(account.id.clone())
                .await?;
            let local = local
                .into_iter()
                .map(|record| (record.id.clone(), record))
                .collect::<BTreeMap<_, _>>();
            let mut devices = ConfigStore::devices(&account.drive, ROOT_NAME).await?;
            if devices.is_empty() {
                if local.is_empty() {
                    return self
                        .catalog
                        .merge_configuration(account.id.clone(), vec![])
                        .await
                        .map_err(Error::from);
                }
                let device = account
                    .drive
                    .create_device(ROOT_NAME, DeviceType::Linux)
                    .await
                    .map_err(request_error(
                        "Couldn’t create the Gallery settings device.",
                    ))?;
                // Resolve the new root with its own share key before writing.
                devices = ConfigStore::devices(&account.drive, ROOT_NAME).await?;
                if !devices.iter().any(|listed| listed.uid == device.uid) {
                    return Err(Error::message(
                        "The new Gallery device isn’t available yet. Try again.",
                    ));
                }
            }
            devices.sort_by_key(|device| device.uid.to_string());
            let mut incoming = BTreeMap::new();
            let mut writer = None;
            for device in devices {
                let store = ConfigStore::open(&account.drive, device, !local.is_empty()).await?;
                for record in store.read_records(&local).await? {
                    incoming.insert(record.id.clone(), record);
                }
                if writer.is_none() {
                    writer = Some(store);
                }
            }
            if let Some(writer) = writer {
                for (id, record) in local {
                    if let Entry::Vacant(entry) = incoming.entry(id) {
                        writer
                            .write(&format!("{}.json", record.id), record.document.as_bytes())
                            .await?;
                        entry.insert(record);
                    }
                }
            }
            self.catalog
                .merge_configuration(account.id.clone(), incoming.into_values().collect())
                .await
                .map_err(Error::from)
        };
        tokio::select! {
            biased;
            _ = account.cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(90), work) =>
                result.map_err(|_| Error::message("Proton took too long to sync Gallery settings."))?,
        }
    }

    /// Round-trip a trial settings record through independent drive sessions.
    /// Uses a temporary root instead of touching a real Gallery device.
    #[cfg(test)]
    async fn verify_settings_storage(&self, reader: &Self) -> Result<()> {
        let _writer_operation = self.operations.read().await;
        let writer = self.auth.account().await?;
        let other = reader.auth.account().await?;
        if writer.id != other.id {
            return Err(Error::message(
                "The storage check needs two sessions of the same account.",
            ));
        }
        let nonce = tempfile::Builder::new()
            .prefix("probe-")
            .tempdir()
            .map_err(|e| Error::message(e.to_string()))?;
        let name = format!(
            "{ROOT_NAME} {}",
            nonce.path().file_name().unwrap().to_string_lossy()
        );
        let device = writer
            .drive
            .create_device(&name, DeviceType::Linux)
            .await
            .map_err(request_error(
                "Couldn’t create the trial Gallery settings device.",
            ))?;
        let result = async {
            use proton_gallery_rs::library::AlbumSettings;
            use proton_gallery_rs::model::{NodeId, SourceId, SourceSpec, VolumeId};
            let album = SourceSpec {
                id: SourceId::new("trial-album")?,
                account: writer.id.clone(),
                volume: VolumeId::new("trial-volume")?,
                root: NodeId::new("trial-album")?,
                label: "Trial album".into(),
            };
            self.catalog.sync_photo_album(album.clone(), vec![]).await?;
            reader
                .catalog
                .sync_photo_album(album.clone(), vec![])
                .await?;
            self.catalog
                .set_album_settings(
                    album.id.clone(),
                    AlbumSettings {
                        hidden: true,
                        ..Default::default()
                    },
                )
                .await?;
            let writer_device = ConfigStore::devices(&writer.drive, &name)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| Error::message("Couldn’t discover the trial settings device."))?;
            let store = ConfigStore::open(&writer.drive, writer_device, true).await?;
            let scope = other
                .drive
                .get_my_files_folder()
                .await
                .map_err(request_error(
                    "Couldn’t find the trial configuration change feed.",
                ))?
                .tree_event_scope_id();
            let head = other
                .drive
                .enumerate_events(&scope, None)
                .await
                .map_err(request_error(
                    "Couldn’t seed the trial configuration cursor.",
                ))?
                .pop()
                .ok_or_else(|| Error::message("No trial configuration cursor was returned."))?;
            for record in self
                .catalog
                .configuration_records(writer.id.clone())
                .await?
            {
                store
                    .write(&format!("{}.json", record.id), record.document.as_bytes())
                    .await?;
            }
            let reader_device = ConfigStore::devices(&other.drive, &name)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    Error::message("The second session couldn’t find the trial settings device.")
                })?;
            let store = ConfigStore::open(&other.drive, reader_device, false).await?;
            let mut signaled = false;
            let mut cursor = head.id().clone();
            for _ in 0..6 {
                let events = store
                    .drive
                    .enumerate_events(&scope, Some(&cursor))
                    .await
                    .map_err(request_error("Couldn’t read trial configuration changes."))?;
                if events.iter().any(|event| {
                    matches!(event, DriveEvent::NodeUpdated {
                    parent_node_uid: Some(parent), ..
                } if parent == &store.root)
                }) {
                    signaled = true;
                    break;
                }
                if let Some(event) = events.last() {
                    cursor = event.id().clone();
                }
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            if !signaled {
                return Err(Error::message(
                    "The Drive feed did not report the settings file change.",
                ));
            }
            let records = store.read_records(&BTreeMap::new()).await?;
            reader
                .catalog
                .merge_configuration(other.id, records)
                .await?;
            if !reader.catalog.album_settings(album.id).await?.hidden {
                return Err(Error::message(
                    "The second session did not apply the synced hide.",
                ));
            }
            Ok(())
        }
        .await;
        let cleanup = writer
            .drive
            .delete_device(&device.uid)
            .await
            .map_err(request_error(
                "Couldn’t remove the trial Gallery settings device.",
            ));
        if let Err(error) = &cleanup {
            tracing::warn!("{error}");
        }
        result.and(cleanup)
    }
}

#[cfg(test)]
mod tests {
    use crate::{SignInStage, testing::*};
    use proton_drive_rs::ProtonDrivePublicLinkClient;
    use std::sync::Arc;

    async fn sign_in(app: &crate::App) {
        let username = std::env::var("PG_TEST_USERNAME").expect("test account name");
        let password = std::env::var("PG_TEST_PASSWORD").expect("test account password");
        let mut state = app.sign_in(username, password.into()).await.unwrap();
        if state.stage == SignInStage::SecondFactor {
            let code = std::env::var("PG_TEST_TOTP_CODE").expect("test account one-time code");
            state = app.submit_second_factor(code).await.unwrap();
        }
        assert_eq!(state.stage, SignInStage::Connected);
    }

    #[tokio::test]
    #[ignore = "writes a temporary Gallery device to a disposable Proton account"]
    async fn live_gallery_settings_device_storage() {
        let vault: Arc<MemoryVault> = Arc::default();
        let (_first_dir, first) = app(vault.clone()).await;
        sign_in(&first).await;
        let (_second_dir, second) = app(vault).await;
        assert_eq!(
            second.restore_session().await.unwrap().stage,
            SignInStage::Connected
        );
        first.verify_settings_storage(&second).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "creates and revokes a public link to an empty disposable Drive folder"]
    async fn live_gallery_drive_folder_link() {
        use crate::sharing::PublicLinkAction;
        use proton_gallery_rs::model::{NodeId, SourceSpec, VolumeId};
        let (_dir, app) = app(Arc::default()).await;
        sign_in(&app).await;
        let account = app.auth.account().await.unwrap();
        app.catalog
            .import_gallery_configuration(account.id.clone())
            .await
            .unwrap();
        app.catalog
            .verify_gallery_visibility(account.id.clone())
            .await
            .unwrap();
        let nonce = tempfile::tempdir().unwrap();
        let name = format!(
            "Gallery link probe {}",
            nonce.path().file_name().unwrap().to_string_lossy()
        );
        let root = account.drive.get_my_files_folder().await.unwrap();
        let folder = account
            .drive
            .create_folder(&root.uid, &name, None)
            .await
            .unwrap();
        let result = async {
            let node = account
                .drive
                .get_node(&folder)
                .await
                .map_err(super::request_error("Couldn’t load the trial folder."))?
                .ok_or(proton_gallery_rs::Error::NotFound)?;
            let id = proton_gallery_drive_rs::folder_source_id(&account.id, &node)?;
            app.catalog
                .register_album(
                    id.clone(),
                    SourceSpec {
                        id: id.clone(),
                        account: account.id.clone(),
                        volume: VolumeId::new(folder.volume_id.as_str())?,
                        root: NodeId::new(folder.link_id.as_str())?,
                        label: name,
                    },
                    None,
                )
                .await?;
            let password = "gallery-test-password";
            let expiry = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 86400;
            let link = app
                .drive_public_link(
                    id.to_string(),
                    id.to_string(),
                    PublicLinkAction::Create {
                        password: password.to_owned().into(),
                        expires: Some(expiry),
                    },
                )
                .await?
                .ok_or_else(|| super::Error::message("No link was returned."))?;
            let visited = async {
                let url = link
                    .url
                    .as_deref()
                    .ok_or_else(|| super::Error::message("No link URL was returned."))?;
                let visitor = ProtonDrivePublicLinkClient::open(
                    proton_gallery_drive_rs::client_configuration(),
                    url,
                    Some(password),
                )
                .await
                .map_err(|_| {
                    super::Error::message("A signed-out client couldn’t open the Drive link.")
                })?;
                let root = visitor.get_root_node().await.map_err(|_| {
                    super::Error::message("A signed-out client couldn’t read the Drive folder.")
                })?;
                if root.uid != folder || !link.has_custom_password || link.expiration_time.is_none()
                {
                    return Err(super::Error::message(
                        "The link opened the wrong folder or did not use the requested settings.",
                    ));
                }
                Ok(())
            }
            .await;
            app.drive_public_link(id.to_string(), id.to_string(), PublicLinkAction::Revoke)
                .await?;
            visited?;
            if let Some(url) = link.url.as_deref()
                && ProtonDrivePublicLinkClient::open(
                    proton_gallery_drive_rs::client_configuration(),
                    url,
                    Some(password),
                )
                .await
                .is_ok()
            {
                return Err(super::Error::message("The revoked link still opens."));
            }
            Ok::<_, super::Error>(())
        }
        .await;
        account
            .drive
            .trash_nodes(std::slice::from_ref(&folder))
            .await
            .unwrap()
            .pop()
            .unwrap()
            .1
            .unwrap();
        account
            .drive
            .delete_nodes(&[folder])
            .await
            .unwrap()
            .pop()
            .unwrap()
            .1
            .unwrap();
        result.unwrap();
    }
}
