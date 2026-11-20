use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::*;
use proton_gallery_rs::phone::*;

fn account() -> AccountId {
    AccountId::new("account").unwrap()
}

fn album(id: &str) -> SourceSpec {
    SourceSpec {
        id: SourceId::new(id).unwrap(),
        account: account(),
        volume: VolumeId::new("photos-volume").unwrap(),
        root: NodeId::new(format!("{id}-node")).unwrap(),
        label: id.into(),
    }
}

fn media(id: &str, folder: &str) -> PhoneMedia {
    PhoneMedia {
        id: id.into(),
        name: format!("IMG_{id}.jpg"),
        mime: "image/jpeg".into(),
        folder: folder.into(),
        folder_name: folder.into(),
        taken: 1,
        modified: 1,
        size_bytes: 10,
        width: 4,
        height: 3,
        favorite: false,
    }
}

async fn back_up(catalog: &Catalog, phone: &str) {
    let copy = BackupState::BackedUp {
        volume: VolumeId::new("photos-volume").unwrap(),
        node: NodeId::new(format!("r{phone}")).unwrap(),
    };
    catalog
        .phone_mark(account(), vec![(phone.into(), copy)])
        .await
        .unwrap();
}

async fn queued(catalog: &Catalog) -> Vec<(String, String)> {
    catalog
        .album_adds(account(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|add| (add.phone, add.album.to_string()))
        .collect()
}

#[tokio::test]
async fn backups_go_into_their_folders_albums_once_uploaded() {
    let catalog = Catalog::in_memory().await.unwrap();
    catalog
        .register_source(phone_source().unwrap())
        .await
        .unwrap();
    catalog
        .phone_upsert(
            1,
            vec![
                media("1", "camera"),
                media("2", "camera"),
                media("3", "screenshots"),
            ],
        )
        .await
        .unwrap();
    catalog
        .set_backup_folder("camera".into(), true)
        .await
        .unwrap();
    for id in ["album-camera", "album-pixel"] {
        catalog.sync_photo_album(album(id), vec![]).await.unwrap();
    }
    let camera = SourceId::new("album-camera").unwrap();
    let pixel = SourceId::new("album-pixel").unwrap();
    catalog
        .set_backup_album(account(), Some("camera".into()), camera.clone(), true)
        .await
        .unwrap();
    catalog
        .set_backup_album(account(), None, pixel.clone(), true)
        .await
        .unwrap();
    // Only albums of this account.
    assert!(
        catalog
            .set_backup_album(account(), None, SourceId::new("phone").unwrap(), true)
            .await
            .is_err()
    );
    assert_eq!(catalog.backup_albums(account()).await.unwrap().len(), 2);

    // Uploaded: queued for the folder's album and the every-backup album.
    back_up(&catalog, "1").await;
    catalog
        .queue_backup_albums(account(), "1".into())
        .await
        .unwrap();
    catalog
        .queue_backup_albums(account(), "3".into())
        .await
        .unwrap();
    assert_eq!(
        queued(&catalog).await,
        [
            ("1".into(), "album-camera".into()),
            ("1".into(), "album-pixel".into()),
        ]
    );
    // Screenshot 3 is queued for the every-backup album but waits for its upload.
    back_up(&catalog, "3").await;
    assert_eq!(queued(&catalog).await.len(), 3);

    // Asked into an album before it is backed up: waits for the upload.
    assert_eq!(
        catalog
            .queue_album_adds(account(), vec!["2".into(), "gone".into()], camera.clone())
            .await
            .unwrap(),
        1
    );
    assert_eq!(queued(&catalog).await.len(), 3);
    back_up(&catalog, "2").await;
    assert_eq!(queued(&catalog).await.len(), 4);

    // Done adds leave the queue; a failed one stays for the next backup.
    let adds = catalog.album_adds(account(), 100).await.unwrap();
    assert_eq!(adds[0].remote.node.as_str(), "r1");
    catalog
        .finish_album_adds(
            account(),
            adds.iter()
                .filter(|a| a.phone != "2")
                .map(|a| (a.phone.clone(), a.album.clone()))
                .collect(),
        )
        .await
        .unwrap();
    assert_eq!(
        queued(&catalog).await,
        [("2".into(), "album-camera".into())]
    );

    // Photos backed up before the album was chosen.
    let catalog_count = catalog
        .queue_backed_up(account(), Some("camera".into()), pixel.clone())
        .await
        .unwrap();
    assert_eq!(catalog_count, 2);

    // Deleted in Proton: its mapping and queue go.
    catalog.forget_photo_album(camera.clone()).await.unwrap();
    assert_eq!(
        catalog.backup_albums(account()).await.unwrap(),
        [proton_gallery_rs::backup_albums::BackupAlbum {
            folder: None,
            album: pixel.clone()
        }]
    );
    assert!(
        queued(&catalog)
            .await
            .iter()
            .all(|(_, album)| album == "album-pixel")
    );

    // Stopped.
    catalog
        .set_backup_album(account(), None, pixel, false)
        .await
        .unwrap();
    assert!(catalog.backup_albums(account()).await.unwrap().is_empty());
}

#[tokio::test]
async fn signing_out_forgets_the_accounts_records_and_keeps_the_phones() {
    let catalog = Catalog::in_memory().await.unwrap();
    catalog
        .register_source(phone_source().unwrap())
        .await
        .unwrap();
    catalog
        .phone_upsert(1, vec![media("1", "camera")])
        .await
        .unwrap();
    let photos = album("photos-1");
    catalog.register_source(photos.clone()).await.unwrap();
    catalog
        .set_source_kind(
            photos.id.clone(),
            proton_gallery_rs::library::SourceKind::Photos,
        )
        .await
        .unwrap();
    let asset = Asset {
        key: AssetKey {
            account: account(),
            volume: VolumeId::new("photos-volume").unwrap(),
            node: NodeId::new("r1").unwrap(),
        },
        name: "secret-name.jpg".into(),
        kind: MediaKind::Image,
        mime: "image/jpeg".into(),
        revision: None,
        size_bytes: Some(1),
        sort_time: 1,
        favorite: false,
    };
    catalog
        .add_to_source(photos.id.clone(), asset.clone())
        .await
        .unwrap();
    let private = album("album-private");
    catalog
        .sync_photo_album(private.clone(), vec![asset])
        .await
        .unwrap();
    back_up(&catalog, "1").await;
    catalog
        .set_backup_album(account(), None, private.id.clone(), true)
        .await
        .unwrap();
    let before = catalog.picker_state().await.unwrap();

    catalog.forget_account(account()).await.unwrap();
    assert!(catalog.photo_albums(account()).await.unwrap().is_empty());
    assert_eq!(catalog.source_count(photos.id.clone()).await.unwrap(), 0);
    assert!(catalog.backup_albums(account()).await.unwrap().is_empty());
    assert_ne!(
        catalog.picker_state().await.unwrap().collection,
        before.collection
    );
    let outline = catalog
        .timeline_outline(None, Some(account()))
        .await
        .unwrap();
    // Only the phone's own photo is left, no longer backed up.
    assert_eq!(outline.len(), 1);
    assert_eq!(outline[0].key.node.as_str(), "1");
    assert_eq!(
        catalog
            .phone_entry("1".into(), Some(account()))
            .await
            .unwrap()
            .unwrap()
            .state,
        BackupState::Unchecked
    );
}
