use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::library::SourceKind;
use proton_gallery_rs::model::*;

fn account() -> AccountId {
    AccountId::new("account").unwrap()
}

fn photo(node: &str, time: i64) -> Asset {
    Asset {
        key: AssetKey {
            account: account(),
            volume: VolumeId::new("photos-volume").unwrap(),
            node: NodeId::new(node).unwrap(),
        },
        name: format!("{node}.jpg"),
        kind: MediaKind::Image,
        mime: "image/jpeg".into(),
        revision: None,
        size_bytes: Some(1),
        sort_time: time,
        favorite: false,
    }
}

fn spec(id: &str, label: &str) -> SourceSpec {
    SourceSpec {
        id: SourceId::new(id).unwrap(),
        account: account(),
        volume: VolumeId::new("photos-volume").unwrap(),
        root: NodeId::new(format!("{id}-node")).unwrap(),
        label: label.into(),
    }
}

#[tokio::test]
async fn proton_albums_list_on_their_own_and_follow_proton() {
    let catalog = Catalog::in_memory().await.unwrap();
    // A Drive gallery, which must stay out of the album list and vice versa.
    let drive = spec("drive-1", "Trips");
    catalog
        .register_album(drive.id.clone(), drive.clone(), None)
        .await
        .unwrap();

    let summer = spec("album-summer", "Summer");
    catalog
        .sync_photo_album(summer.clone(), vec![photo("a", 1), photo("b", 2)])
        .await
        .unwrap();
    let albums = catalog.photo_albums(account()).await.unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].source.label, "Summer");
    assert_eq!(albums[0].direct_items, 2);
    assert_eq!(albums[0].cover.as_ref().unwrap().name, "b.jpg");
    let roots = catalog
        .albums_scoped(None, None, Some(account()))
        .await
        .unwrap();
    assert_eq!(
        roots
            .iter()
            .map(|a| a.source.label.as_str())
            .collect::<Vec<_>>(),
        ["Trips"]
    );

    // Renamed and changed in Proton.
    catalog
        .sync_photo_album(
            spec("album-summer", "Summer 2026"),
            vec![photo("b", 2), photo("c", 3)],
        )
        .await
        .unwrap();
    let albums = catalog.photo_albums(account()).await.unwrap();
    assert_eq!(albums[0].source.label, "Summer 2026");
    assert_eq!(albums[0].direct_items, 2);

    // Changed here.
    assert_eq!(
        catalog
            .add_album_members(
                summer.id.clone(),
                vec![photo("a", 1).key, photo("unknown", 9).key]
            )
            .await
            .unwrap(),
        1,
        "only photos the catalog knows"
    );
    assert_eq!(
        catalog
            .remove_album_members(summer.id.clone(), vec![photo("b", 2).key])
            .await
            .unwrap(),
        1
    );
    assert_eq!(catalog.source_count(summer.id.clone()).await.unwrap(), 2);

    // Deleted in Proton: gone from the list, its photos stay as assets.
    catalog
        .retain_photo_albums(account(), vec![])
        .await
        .unwrap();
    assert!(catalog.photo_albums(account()).await.unwrap().is_empty());
    assert!(
        catalog
            .assets_by_key(vec![photo("c", 3).key])
            .await
            .unwrap()[0]
            .is_some()
    );
    assert_eq!(
        catalog
            .albums_scoped(None, None, Some(account()))
            .await
            .unwrap()
            .len(),
        1
    );

    // An album id cannot be read as another account's.
    let other = AccountId::new("other").unwrap();
    assert!(
        catalog
            .photo_album(other, summer.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    let _ = SourceKind::Album;
}
