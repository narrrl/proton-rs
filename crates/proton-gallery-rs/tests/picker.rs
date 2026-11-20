use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::library::SourceKind;
use proton_gallery_rs::model::*;
use proton_gallery_rs::phone::*;
use proton_gallery_rs::picker::*;

fn account() -> AccountId {
    AccountId::new("account").unwrap()
}

fn remote(node: &str, name: &str, mime: &str, time: i64) -> Asset {
    Asset {
        key: AssetKey {
            account: account(),
            volume: VolumeId::new("photos-volume").unwrap(),
            node: NodeId::new(node).unwrap(),
        },
        name: name.into(),
        kind: if name.ends_with(".mp4") {
            MediaKind::Video
        } else {
            MediaKind::Image
        },
        mime: mime.into(),
        revision: None,
        size_bytes: Some(1000),
        sort_time: time,
        favorite: false,
    }
}

fn spec(id: &str) -> SourceSpec {
    SourceSpec {
        id: SourceId::new(id).unwrap(),
        account: account(),
        volume: VolumeId::new("photos-volume").unwrap(),
        root: NodeId::new("root").unwrap(),
        label: id.into(),
    }
}

async fn setup() -> (Catalog, SourceSpec) {
    let catalog = Catalog::in_memory().await.unwrap();
    catalog
        .register_source(phone_source().unwrap())
        .await
        .unwrap();
    let photos = spec("photos-1");
    catalog.register_source(photos.clone()).await.unwrap();
    catalog
        .set_source_kind(photos.id.clone(), SourceKind::Photos)
        .await
        .unwrap();
    (catalog, photos)
}

async fn changed(
    catalog: &Catalog,
    photos: &SourceSpec,
    after: u64,
) -> Vec<(String, Option<String>)> {
    catalog
        .picker_changes(photos.id.clone(), after, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| (entry.asset.name, entry.phone))
        .collect()
}

#[tokio::test]
async fn the_picker_only_reads_what_changed_since_it_last_looked() {
    let (catalog, photos) = setup().await;
    let empty = catalog.picker_state().await.unwrap();
    assert_eq!(empty.generation, 0);
    assert_eq!(empty.collection.len(), 32);

    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r1", "IMG_1.jpg", "image/jpeg", 100),
        )
        .await
        .unwrap();
    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r2", "IMG_2.jpg", "image/jpeg", 200),
        )
        .await
        .unwrap();
    let first = catalog.picker_state().await.unwrap();
    assert_eq!(first.collection, empty.collection);
    assert_eq!(changed(&catalog, &photos, 0).await.len(), 2);
    assert!(
        changed(&catalog, &photos, first.generation)
            .await
            .is_empty()
    );

    // Syncing the same metadata again is not a change.
    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r1", "IMG_1.jpg", "image/jpeg", 100),
        )
        .await
        .unwrap();
    assert_eq!(catalog.picker_state().await.unwrap(), first);

    // A renamed photo is.
    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r1", "Beach.jpg", "image/jpeg", 100),
        )
        .await
        .unwrap();
    assert_eq!(
        changed(&catalog, &photos, first.generation).await,
        [("Beach.jpg".into(), None)]
    );

    // Drive galleries are not part of the picker's library.
    let drive = spec("drive-1");
    catalog.register_source(drive.clone()).await.unwrap();
    catalog
        .set_source_kind(drive.id.clone(), SourceKind::Drive)
        .await
        .unwrap();
    let before = catalog.picker_state().await.unwrap().generation;
    let mut other = remote("d1", "Scan.jpg", "image/jpeg", 300);
    other.key.volume = VolumeId::new("drive-volume").unwrap();
    let mut drive_spec = drive.clone();
    drive_spec.volume = other.key.volume.clone();
    catalog
        .add_to_source(drive.id.clone(), other)
        .await
        .unwrap();
    assert_eq!(catalog.picker_state().await.unwrap().generation, before);

    // Lookups by node only reach the Photos source.
    assert!(
        catalog
            .picker_asset(photos.clone(), NodeId::new("r2").unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        catalog
            .picker_asset(photos.clone(), NodeId::new("d1").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .picker_asset(drive_spec, NodeId::new("r2").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_phone_copy_appearing_or_leaving_updates_the_picker() {
    let (catalog, photos) = setup().await;
    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r1", "IMG_1.jpg", "image/jpeg", 100),
        )
        .await
        .unwrap();
    catalog
        .phone_upsert(
            1,
            vec![PhoneMedia {
                id: "7".into(),
                name: "IMG_1.jpg".into(),
                mime: "image/jpeg".into(),
                folder: "camera".into(),
                folder_name: "Camera".into(),
                taken: 100,
                modified: 100,
                size_bytes: 1000,
                width: 4,
                height: 3,
                favorite: false,
            }],
        )
        .await
        .unwrap();
    let before = catalog.picker_state().await.unwrap().generation;
    let copy = BackupState::BackedUp {
        volume: VolumeId::new("photos-volume").unwrap(),
        node: NodeId::new("r1").unwrap(),
    };
    catalog
        .phone_mark(account(), vec![("7".into(), copy)])
        .await
        .unwrap();
    assert_eq!(
        changed(&catalog, &photos, before).await,
        [("IMG_1.jpg".into(), Some("7".into()))]
    );

    // Freed up: the phone file is gone, the Proton photo shows again.
    let backed_up = catalog.picker_state().await.unwrap().generation;
    catalog.phone_finish(2).await.unwrap();
    assert_eq!(
        changed(&catalog, &photos, backed_up).await,
        [("IMG_1.jpg".into(), None)]
    );
}

#[test]
fn every_item_has_a_mime_type_the_picker_can_filter_on() {
    assert_eq!(picker_mime(&remote("a", "a.HEIC", "", 0)), "image/heic");
    assert_eq!(
        picker_mime(&remote("a", "clip.mp4", "application/octet-stream", 0)),
        "video/mp4"
    );
    assert_eq!(picker_mime(&remote("a", "noext", "", 0)), "image/jpeg");
    assert_eq!(
        picker_mime(&remote("a", "a.jpg", "image/png", 0)),
        "image/png"
    );
}

#[tokio::test]
async fn trashed_and_vanished_photos_leave_the_timeline_and_the_picker() {
    let (catalog, photos) = setup().await;
    for (node, name, time) in [("r1", "a.jpg", 1), ("r2", "b.jpg", 2), ("r3", "c.jpg", 3)] {
        catalog
            .add_to_source(photos.id.clone(), remote(node, name, "image/jpeg", time))
            .await
            .unwrap();
    }
    let before = catalog.picker_state().await.unwrap().generation;

    // Trashed in this app.
    let trashed = remote("r1", "a.jpg", "image/jpeg", 1).key;
    assert_eq!(
        catalog
            .remove_assets(account(), vec![trashed])
            .await
            .unwrap(),
        1
    );
    // Gone from Proton at the next complete sync.
    assert_eq!(
        catalog
            .prune_source(photos.id.clone(), vec![NodeId::new("r3").unwrap()])
            .await
            .unwrap(),
        1
    );
    assert_eq!(catalog.source_count(photos.id.clone()).await.unwrap(), 1);
    let deleted: Vec<String> = catalog
        .picker_deletions(photos.clone(), before, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|(node, _)| node.to_string())
        .collect();
    assert_eq!(deleted, ["r1", "r2"]);
    assert!(changed(&catalog, &photos, before).await.is_empty());

    // Restored in Proton and synced again: listed, no longer deleted.
    let seen = catalog.picker_state().await.unwrap().generation;
    catalog
        .add_to_source(photos.id.clone(), remote("r2", "b.jpg", "image/jpeg", 2))
        .await
        .unwrap();
    assert_eq!(
        changed(&catalog, &photos, seen).await,
        [("b.jpg".into(), None)]
    );
    let deleted = catalog
        .picker_deletions(photos.clone(), before, 10)
        .await
        .unwrap();
    assert_eq!(deleted.len(), 1);

    // Another account's items are refused.
    let mut foreign = remote("r3", "c.jpg", "image/jpeg", 3).key;
    foreign.account = AccountId::new("other").unwrap();
    assert!(
        catalog
            .remove_assets(account(), vec![foreign])
            .await
            .is_err()
    );
}

fn album(id: &str) -> SourceSpec {
    SourceSpec {
        label: "Private".into(),
        ..spec(id)
    }
}

async fn hide(catalog: &Catalog, source: &SourceSpec, hidden: bool) {
    let settings = proton_gallery_rs::library::AlbumSettings {
        hidden,
        ..catalog.album_settings(source.id.clone()).await.unwrap()
    };
    catalog
        .set_album_settings(source.id.clone(), settings)
        .await
        .unwrap();
}

async fn timeline(catalog: &Catalog, photos: &SourceSpec) -> Vec<String> {
    catalog
        .timeline_outline(Some(photos.id.clone()), Some(account()))
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.key.node.to_string())
        .collect()
}

#[tokio::test]
async fn media_in_a_hidden_album_leaves_the_timeline_and_the_picker() {
    let (catalog, photos) = setup().await;
    for (node, name, time) in [("r1", "a.jpg", 1), ("r2", "b.jpg", 2)] {
        catalog
            .add_to_source(photos.id.clone(), remote(node, name, "image/jpeg", time))
            .await
            .unwrap();
    }
    // A phone photo whose Proton copy goes into the hidden album.
    catalog
        .phone_upsert(
            1,
            vec![PhoneMedia {
                id: "7".into(),
                name: "IMG_7.jpg".into(),
                mime: "image/jpeg".into(),
                folder: "camera".into(),
                folder_name: "Camera".into(),
                taken: 3,
                modified: 3,
                size_bytes: 1000,
                width: 4,
                height: 3,
                favorite: false,
            }],
        )
        .await
        .unwrap();
    assert_eq!(timeline(&catalog, &photos).await, ["7", "r2", "r1"]);

    let private = album("album-private");
    catalog
        .sync_photo_album(
            private.clone(),
            vec![remote("r1", "a.jpg", "image/jpeg", 1)],
        )
        .await
        .unwrap();
    let before = catalog.picker_state().await.unwrap().generation;
    hide(&catalog, &private, true).await;

    // Hiding is a change: the picker hears that r1 is gone.
    assert_eq!(timeline(&catalog, &photos).await, ["7", "r2"]);
    let deleted: Vec<String> = catalog
        .picker_deletions(photos.clone(), before, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|(node, _)| node.to_string())
        .collect();
    assert_eq!(deleted, ["r1"]);
    assert!(
        changed(&catalog, &photos, 0)
            .await
            .iter()
            .all(|(name, _)| name != "a.jpg")
    );
    assert!(
        catalog
            .picker_asset(photos.clone(), NodeId::new("r1").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    // The album itself still shows it.
    assert_eq!(catalog.source_count(private.id.clone()).await.unwrap(), 1);
    assert_eq!(
        catalog
            .timeline_count(Some(photos.id.clone()), Some(account()))
            .await
            .unwrap(),
        2
    );

    // Backed up, then moved into the hidden album: the phone original
    // leaves the timeline with its copy.
    catalog
        .add_to_source(
            photos.id.clone(),
            remote("r3", "IMG_7.jpg", "image/jpeg", 3),
        )
        .await
        .unwrap();
    let copy = BackupState::BackedUp {
        volume: VolumeId::new("photos-volume").unwrap(),
        node: NodeId::new("r3").unwrap(),
    };
    catalog
        .phone_mark(account(), vec![("7".into(), copy)])
        .await
        .unwrap();
    assert_eq!(timeline(&catalog, &photos).await, ["r3", "r2"]);
    let joined = catalog.picker_state().await.unwrap().generation;
    catalog
        .add_album_members(
            private.id.clone(),
            vec![remote("r3", "IMG_7.jpg", "image/jpeg", 3).key],
        )
        .await
        .unwrap();
    assert_eq!(timeline(&catalog, &photos).await, ["r2"]);
    let deleted = catalog
        .picker_deletions(photos.clone(), joined, 10)
        .await
        .unwrap();
    assert_eq!(deleted.len(), 1);

    // Shown again: everything is back, and the picker lists it anew.
    let hidden = catalog.picker_state().await.unwrap().generation;
    hide(&catalog, &private, false).await;
    assert_eq!(timeline(&catalog, &photos).await, ["r3", "r2", "r1"]);
    let mut back: Vec<String> = changed(&catalog, &photos, hidden)
        .await
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    back.sort();
    assert_eq!(back, ["IMG_7.jpg", "a.jpg"]);

    // The timeline and the phone cannot be hidden.
    let settings = proton_gallery_rs::library::AlbumSettings {
        hidden: true,
        ..Default::default()
    };
    assert!(
        catalog
            .set_album_settings(photos.id.clone(), settings.clone())
            .await
            .is_err()
    );
    assert!(
        catalog
            .set_album_settings(SourceId::new("phone").unwrap(), settings)
            .await
            .is_err()
    );
}
