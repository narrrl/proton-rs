use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::*;
use proton_gallery_rs::phone::*;

fn media(id: &str, name: &str, taken: i64) -> PhoneMedia {
    PhoneMedia {
        id: id.into(),
        name: name.into(),
        mime: "image/jpeg".into(),
        folder: "camera".into(),
        folder_name: "Camera".into(),
        taken,
        modified: taken,
        size_bytes: 1000,
        width: 4,
        height: 3,
        favorite: false,
    }
}

fn account() -> AccountId {
    AccountId::new("account").unwrap()
}

fn remote(node: &str, name: &str, time: i64) -> Asset {
    Asset {
        key: AssetKey {
            account: account(),
            volume: VolumeId::new("photos-volume").unwrap(),
            node: NodeId::new(node).unwrap(),
        },
        name: name.into(),
        kind: MediaKind::Image,
        mime: "image/jpeg".into(),
        revision: None,
        size_bytes: Some(1000),
        sort_time: time,
        favorite: false,
    }
}

async fn setup() -> (Catalog, SourceId) {
    let catalog = Catalog::in_memory().await.unwrap();
    catalog
        .register_source(phone_source().unwrap())
        .await
        .unwrap();
    let photos = SourceSpec {
        id: SourceId::new("photos-1").unwrap(),
        account: account(),
        volume: VolumeId::new("photos-volume").unwrap(),
        root: NodeId::new("root").unwrap(),
        label: "Proton Photos".into(),
    };
    catalog.register_source(photos.clone()).await.unwrap();
    (catalog, photos.id)
}

async fn names(catalog: &Catalog, photos: &SourceId, account: Option<AccountId>) -> Vec<String> {
    let page = catalog
        .timeline_window(
            Some(photos.clone()),
            account,
            None,
            false,
            PageSize::new(50).unwrap(),
        )
        .await
        .unwrap();
    page.items
        .into_iter()
        .map(|a| {
            format!(
                "{}:{}",
                if is_phone(&a.key) { "phone" } else { "proton" },
                a.name
            )
        })
        .collect()
}

#[tokio::test]
async fn the_timeline_shows_each_photo_once_whether_on_the_phone_in_proton_or_both() {
    let (catalog, photos) = setup().await;
    catalog
        .add_to_source(photos.clone(), remote("r1", "IMG_1.jpg", 100))
        .await
        .unwrap();
    catalog
        .phone_upsert(
            1,
            vec![media("1", "IMG_1.jpg", 100), media("2", "IMG_2.jpg", 200)],
        )
        .await
        .unwrap();

    // Signed out: everything on the phone is shown.
    assert_eq!(
        names(&catalog, &photos, None).await,
        ["phone:IMG_2.jpg", "phone:IMG_1.jpg", "proton:IMG_1.jpg"]
    );
    // Signed in, before the check: a same-named Proton photo hides the phone copy.
    let signed_in = Some(account());
    assert_eq!(
        names(&catalog, &photos, signed_in.clone()).await,
        ["phone:IMG_2.jpg", "proton:IMG_1.jpg"]
    );
    assert_eq!(
        catalog
            .timeline_count(Some(photos.clone()), signed_in.clone())
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        catalog.phone_unchecked(account(), 10).await.unwrap().len(),
        2
    );

    // Same name, different contents: both are shown.
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), BackupState::Missing),
                ("2".into(), BackupState::Missing),
            ],
        )
        .await
        .unwrap();
    assert_eq!(names(&catalog, &photos, signed_in.clone()).await.len(), 3);

    // Matched: one tile, and the Proton photo knows its phone copy.
    let copy = BackupState::BackedUp {
        volume: VolumeId::new("photos-volume").unwrap(),
        node: NodeId::new("r1").unwrap(),
    };
    catalog
        .phone_mark(account(), vec![("1".into(), copy)])
        .await
        .unwrap();
    assert_eq!(
        names(&catalog, &photos, signed_in.clone()).await,
        ["phone:IMG_2.jpg", "proton:IMG_1.jpg"]
    );
    let links = catalog
        .phone_links(
            signed_in.clone(),
            vec![
                remote("r1", "", 0).key,
                phone_key("2").unwrap(),
                phone_key("1").unwrap(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        links[0],
        PhoneLink {
            phone: Some("1".into()),
            backed_up: true
        }
    );
    assert_eq!(
        links[1],
        PhoneLink {
            phone: Some("2".into()),
            backed_up: false
        }
    );
    assert!(links[2].backed_up);

    // Only unsaved media in chosen folders is queued.
    assert!(
        catalog
            .backup_queue(account(), 10)
            .await
            .unwrap()
            .is_empty()
    );
    catalog
        .set_backup_folder("camera".into(), true)
        .await
        .unwrap();
    assert_eq!(
        catalog.backup_queue(account(), 10).await.unwrap(),
        [("2".to_string(), false)]
    );
    let folders = catalog.phone_folders(signed_in).await.unwrap();
    assert_eq!(
        (
            folders[0].count,
            folders[0].not_backed_up,
            folders[0].backup
        ),
        (2, 1, true)
    );
}

#[tokio::test]
async fn a_changed_file_is_checked_again_and_a_deleted_one_disappears() {
    let (catalog, photos) = setup().await;
    catalog
        .phone_upsert(1, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog
        .phone_set_hashes(vec![("1".into(), "a".repeat(40))])
        .await
        .unwrap();
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), BackupState::Maybe),
                ("2".into(), BackupState::Missing),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        catalog.phone_hashed(account(), 10).await.unwrap(),
        [("1".into(), "a.jpg".into(), "a".repeat(40))]
    );
    assert!(
        catalog
            .phone_set_hashes(vec![("1".into(), "A".repeat(40))])
            .await
            .is_err()
    );

    let mut edited = media("1", "a.jpg", 10);
    edited.modified = 99;
    catalog.phone_upsert(2, vec![edited]).await.unwrap();
    assert_eq!(catalog.phone_finish(2).await.unwrap(), 1);
    let entry = catalog
        .phone_entry("1".into(), Some(account()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((entry.sha1, entry.state), (None, BackupState::Unchecked));
    assert!(
        catalog
            .phone_entry("2".into(), Some(account()))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(names(&catalog, &photos, None).await, ["phone:a.jpg"]);
}

#[tokio::test]
async fn the_merged_timeline_pages_both_ways_without_gaps() {
    let (catalog, photos) = setup().await;
    for i in 0..40 {
        catalog
            .add_to_source(
                photos.clone(),
                remote(&format!("r{i}"), &format!("p{i}.jpg"), i * 2),
            )
            .await
            .unwrap();
    }
    catalog
        .phone_upsert(
            1,
            (0..40)
                .map(|i| media(&i.to_string(), &format!("m{i}.jpg"), i * 2 + 1))
                .collect(),
        )
        .await
        .unwrap();
    let size = PageSize::new(25).unwrap();
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let page = catalog
            .timeline_window(
                Some(photos.clone()),
                Some(account()),
                cursor.clone(),
                false,
                size,
            )
            .await
            .unwrap();
        pages.push(page.items.clone());
        seen.extend(page.items.iter().map(|a| a.sort_time));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, (0..80).rev().collect::<Vec<_>>());
    let last = catalog
        .timeline_window(Some(photos.clone()), Some(account()), cursor, false, size)
        .await;
    assert!(last.is_ok());
    let first = catalog
        .timeline_window(Some(photos.clone()), Some(account()), None, false, size)
        .await
        .unwrap();
    let second = catalog
        .timeline_window(
            Some(photos.clone()),
            Some(account()),
            first.next,
            false,
            size,
        )
        .await
        .unwrap();
    assert_eq!(second.items, pages[1]);
    let back = catalog
        .timeline_window(
            Some(photos.clone()),
            Some(account()),
            second.previous,
            true,
            size,
        )
        .await
        .unwrap();
    assert_eq!(back.items, pages[0]);
    let other = catalog
        .window(Some(photos.clone()), None, false, size)
        .await
        .unwrap()
        .next
        .unwrap();
    assert!(
        catalog
            .timeline_window(Some(photos), None, Some(other), false, size)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn overlapping_scans_keep_what_was_already_compared() {
    let (catalog, _) = setup().await;
    catalog
        .phone_upsert(1, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog.phone_finish(1).await.unwrap();
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), BackupState::Missing),
                ("2".into(), BackupState::Missing),
            ],
        )
        .await
        .unwrap();
    assert_eq!(catalog.phone_remaining(account()).await.unwrap(), 0);

    // Scan 3 starts while scan 2 is still running and finishes first.
    catalog
        .phone_upsert(3, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog
        .phone_upsert(2, vec![media("1", "a.jpg", 10)])
        .await
        .unwrap();
    assert_eq!(catalog.phone_finish(3).await.unwrap(), 0);
    catalog
        .phone_upsert(2, vec![media("2", "b.jpg", 20)])
        .await
        .unwrap();
    assert_eq!(catalog.phone_finish(2).await.unwrap(), 0);
    assert_eq!(catalog.phone_remaining(account()).await.unwrap(), 0);
    assert_eq!(
        catalog.phone_unchecked(account(), 10).await.unwrap(),
        vec![]
    );
}

#[tokio::test]
async fn a_name_that_appears_in_proton_later_is_compared_again() {
    let (catalog, photos) = setup().await;
    catalog
        .phone_upsert(1, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog.phone_finish(1).await.unwrap();
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), BackupState::Missing),
                ("2".into(), BackupState::Missing),
            ],
        )
        .await
        .unwrap();
    // Item 2's contents were compared: a different photo had its name.
    catalog
        .phone_set_hashes(vec![("2".into(), "b".repeat(40))])
        .await
        .unwrap();
    catalog
        .add_to_source(photos.clone(), remote("r1", "A.JPG", 10))
        .await
        .unwrap();
    catalog
        .add_to_source(photos.clone(), remote("r2", "b.jpg", 20))
        .await
        .unwrap();
    assert_eq!(
        catalog
            .phone_recheck_new_names(account(), photos)
            .await
            .unwrap(),
        1
    );
    let unchecked = catalog.phone_unchecked(account(), 10).await.unwrap();
    assert_eq!(unchecked, vec![("1".to_string(), "a.jpg".to_string())]);
    assert_eq!(catalog.phone_remaining(account()).await.unwrap(), 1);
}

#[tokio::test]
async fn only_verified_older_backups_may_leave_the_phone() {
    let (catalog, photos) = setup().await;
    catalog
        .phone_upsert(
            1,
            vec![
                media("1", "old.jpg", 10),
                media("2", "new.jpg", 500),
                media("3", "unhashed.jpg", 20),
                media("4", "gone.jpg", 30),
            ],
        )
        .await
        .unwrap();
    catalog.phone_finish(1).await.unwrap();
    let hash = "a".repeat(40);
    catalog
        .phone_set_hashes(vec![
            ("1".into(), hash.clone()),
            ("2".into(), hash.clone()),
            ("4".into(), hash.clone()),
        ])
        .await
        .unwrap();
    let copy = |node: &str| BackupState::BackedUp {
        volume: VolumeId::new("photos-volume").unwrap(),
        node: NodeId::new(node).unwrap(),
    };
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), copy("r1")),
                ("2".into(), copy("r2")),
                ("3".into(), copy("r3")),
                ("4".into(), copy("r4")),
            ],
        )
        .await
        .unwrap();
    for (node, name, time) in [
        ("r1", "old.jpg", 10),
        ("r2", "new.jpg", 500),
        ("r3", "unhashed.jpg", 20),
    ] {
        catalog
            .add_to_source(photos.clone(), remote(node, name, time))
            .await
            .unwrap();
    }
    // r4 is not in the synced Photos library, so its phone copy stays.
    let freeable = catalog.freeable(account(), photos, 100).await.unwrap();
    let ids: Vec<_> = freeable.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["1"]);
    assert_eq!(freeable[0].sha1, hash);
    assert_eq!(freeable[0].size_bytes, 1000);
}

#[tokio::test]
async fn a_quick_rescan_forgets_deleted_items_and_reports_unknown_ones() {
    let (catalog, _) = setup().await;
    catalog
        .phone_upsert(1, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog
        .phone_mark(account(), vec![("1".into(), BackupState::Missing)])
        .await
        .unwrap();
    let (removed, unknown) = catalog
        .phone_retain(vec!["2".into(), "3".into(), "3".into()])
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(unknown, ["3"]);
    assert!(
        catalog
            .phone_entry("1".into(), Some(account()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        catalog
            .phone_entry("2".into(), Some(account()))
            .await
            .unwrap()
            .is_some()
    );
    // Nothing changed: nothing to do.
    assert_eq!(
        catalog.phone_retain(vec!["2".into()]).await.unwrap(),
        (0, vec![])
    );
}

#[tokio::test]
async fn files_the_phone_could_not_read_do_not_hold_up_the_check() {
    let (catalog, _) = setup().await;
    catalog
        .phone_upsert(1, vec![media("1", "a.jpg", 10), media("2", "b.jpg", 20)])
        .await
        .unwrap();
    catalog
        .phone_mark(
            account(),
            vec![
                ("1".into(), BackupState::Maybe),
                ("2".into(), BackupState::Maybe),
            ],
        )
        .await
        .unwrap();
    let all = catalog.phone_unhashed(account(), 10, vec![]).await.unwrap();
    assert_eq!(
        all,
        [("2".to_owned(), false), ("1".to_owned(), false)],
        "newest first"
    );
    let rest = catalog
        .phone_unhashed(account(), 10, vec!["2".into()])
        .await
        .unwrap();
    assert_eq!(rest, [("1".to_owned(), false)]);
}

#[tokio::test]
async fn the_outline_lists_the_timeline_in_page_order_with_known_shapes() {
    let (catalog, photos) = setup().await;
    for i in 0..30 {
        catalog
            .add_to_source(
                photos.clone(),
                remote(&format!("r{i}"), &format!("p{i}.jpg"), i * 2),
            )
            .await
            .unwrap();
    }
    catalog
        .phone_upsert(
            1,
            (0..30)
                .map(|i| media(&i.to_string(), &format!("m{i}.jpg"), i * 2 + 1))
                .collect(),
        )
        .await
        .unwrap();
    let remembered = remote("r7", "p7.jpg", 14).key;
    catalog
        .remember_shape(remembered.clone(), 300, 200)
        .await
        .unwrap();

    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = catalog
            .timeline_window(
                Some(photos.clone()),
                Some(account()),
                cursor,
                false,
                PageSize::new(16).unwrap(),
            )
            .await
            .unwrap();
        paged.extend(page.items.into_iter().map(|a| (a.key, a.sort_time)));
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let outline = catalog
        .timeline_outline(Some(photos.clone()), Some(account()))
        .await
        .unwrap();
    assert_eq!(
        outline
            .iter()
            .map(|e| (e.key.clone(), e.sort_time))
            .collect::<Vec<_>>(),
        paged
    );
    let shape = |key: &AssetKey| {
        let entry = outline.iter().find(|e| &e.key == key).unwrap();
        (entry.width, entry.height)
    };
    assert_eq!(shape(&remembered), (300, 200));
    assert_eq!(shape(&remote("r8", "p8.jpg", 16).key), (0, 0));
    // Phone media brings its own size from the scan.
    assert_eq!(shape(&phone_key("3").unwrap()), (4, 3));

    let by_name = catalog
        .source_outline(photos.clone(), SortOrder::Name)
        .await
        .unwrap();
    let names: Vec<_> = by_name
        .iter()
        .map(|e| e.key.node.as_str().to_owned())
        .collect();
    assert_eq!(names.len(), 30);
    let mut sorted = names.clone();
    sorted.sort_by_key(|node| format!("p{}.jpg", &node[1..]));
    assert_eq!(names, sorted);
    assert_eq!(names[2], "r10", "name order, not time order");
}
