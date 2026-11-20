use std::collections::HashSet;

use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::*;
use proton_gallery_rs::source::MediaSource;
use proton_gallery_rs::synthetic::SyntheticSource;
use proton_gallery_rs::{CancellationToken, Error, indexer};

#[tokio::test]
async fn folder_queries_seek_the_source_index_and_shared_asset_time_updates_stay_consistent() {
    use proton_gallery_rs::source::SourcePage;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = Catalog::open(&path).await.unwrap();
    let source = SyntheticSource::new(503).unwrap();
    indexer::scan(&catalog, &source, false, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    let mut second = source.spec().clone();
    second.id = SourceId::new("overlap").unwrap();
    second.root = NodeId::new("other-root").unwrap();
    catalog.register_source(second.clone()).await.unwrap();
    let checkpoint = catalog.begin_scan(second.id.clone(), false).await.unwrap();
    let mut changed = source.asset(500).unwrap();
    changed.sort_time = 9_000_000_000;
    catalog
        .apply_page(
            second,
            checkpoint,
            SourcePage {
                assets: vec![changed.clone()],
                next: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        catalog
            .page(
                Some(source.spec().id.clone()),
                None,
                PageSize::new(1).unwrap()
            )
            .await
            .unwrap()
            .items[0]
            .key,
        changed.key
    );
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut q=conn.prepare("EXPLAIN QUERY PLAN SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node) WHERE m.source=?1 AND (m.sort_time,m.account,m.volume,m.node)<(?2,?3,?4,?5) ORDER BY m.sort_time DESC,m.account DESC,m.volume DESC,m.node DESC LIMIT 90").unwrap();
    let plan = q
        .query_map(
            rusqlite::params![
                source.spec().id.as_str(),
                changed.sort_time,
                changed.key.account.as_str(),
                changed.key.volume.as_str(),
                changed.key.node.as_str()
            ],
            |r| r.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("SEARCH m USING COVERING INDEX memberships_timeline"),
        "{plan}"
    );
    assert!(!plan.contains("TEMP B-TREE"), "{plan}");
}

#[tokio::test]
async fn a_dropped_page_can_be_loaded_backwards_without_gaps() {
    let catalog = Catalog::in_memory().await.unwrap();
    indexer::scan(
        &catalog,
        &SyntheticSource::new(73).unwrap(),
        false,
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    let size = PageSize::new(7).unwrap();
    let first = catalog.window(None, None, false, size).await.unwrap();
    assert!(first.previous.is_none());
    let mut current = catalog
        .window(None, first.next.clone(), false, size)
        .await
        .unwrap();
    let second_keys: Vec<_> = current.items.iter().map(|a| a.key.clone()).collect();
    for _ in 0..5 {
        current = catalog
            .window(None, current.next, false, size)
            .await
            .unwrap();
    }
    for _ in 0..5 {
        current = catalog
            .window(None, current.previous, true, size)
            .await
            .unwrap();
    }
    assert_eq!(
        current
            .items
            .iter()
            .map(|a| a.key.clone())
            .collect::<Vec<_>>(),
        second_keys
    );
    let back = catalog
        .window(None, current.previous, true, size)
        .await
        .unwrap();
    assert!(back.previous.is_none());
    assert_eq!(back.items, first.items);
    assert!(catalog.window(None, None, true, size).await.is_err());
}

#[tokio::test]
async fn version_one_catalog_migrates_without_losing_existing_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("../src/schema_v1.sql"))
        .unwrap();
    conn.execute_batch("PRAGMA user_version=1; PRAGMA application_id=0x50474c59;")
        .unwrap();
    let source = SyntheticSource::new(1).unwrap();
    conn.execute(
        "INSERT INTO sources(id,payload) VALUES (?1,?2)",
        rusqlite::params![
            source.spec().id.as_str(),
            serde_json::to_string(source.spec()).unwrap()
        ],
    )
    .unwrap();
    drop(conn);
    let catalog = Catalog::open(&path).await.unwrap();
    assert_eq!(catalog.local_totals().await.unwrap(), (0, 0));
    assert_eq!(
        catalog.sources().await.unwrap(),
        vec![source.spec().clone()]
    );
    drop(catalog);
    let conn = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        14
    );
}

#[tokio::test]
async fn version_six_catalog_lists_existing_photos_for_the_picker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v6.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    for schema in [
        include_str!("../src/schema_v1.sql"),
        include_str!("../src/schema_v2.sql"),
        include_str!("../src/schema_v3.sql"),
        include_str!("../src/schema_v4.sql"),
        include_str!("../src/schema_v5.sql"),
        include_str!("../src/schema_v6.sql"),
    ] {
        conn.execute_batch(schema).unwrap();
    }
    conn.execute_batch("PRAGMA user_version=6; PRAGMA application_id=0x50474c59;")
        .unwrap();
    let source = SyntheticSource::new(3).unwrap();
    conn.execute(
        "INSERT INTO sources(id,payload) VALUES (?1,?2)",
        rusqlite::params![
            source.spec().id.as_str(),
            serde_json::to_string(source.spec()).unwrap()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO source_kinds(source,kind) VALUES (?1,'photos')",
        [source.spec().id.as_str()],
    )
    .unwrap();
    for (index, node) in ["n1", "n2", "n3"].into_iter().enumerate() {
        let spec = source.spec();
        let asset = Asset {
            key: AssetKey {
                account: spec.account.clone(),
                volume: spec.volume.clone(),
                node: NodeId::new(node).unwrap(),
            },
            name: format!("{node}.jpg"),
            kind: MediaKind::Image,
            mime: "image/jpeg".into(),
            revision: None,
            size_bytes: Some(1),
            sort_time: index as i64,
            favorite: false,
        };
        conn.execute(
            "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                spec.account.as_str(),
                spec.volume.as_str(),
                node,
                index as i64,
                serde_json::to_string(&asset).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)",
            rusqlite::params![
                spec.id.as_str(),
                spec.account.as_str(),
                spec.volume.as_str(),
                node
            ],
        )
        .unwrap();
    }
    drop(conn);

    // Media already in the catalog before the upgrade is listed, oldest first.
    let catalog = Catalog::open(&path).await.unwrap();
    assert_eq!(catalog.picker_state().await.unwrap().generation, 3);
    let listed: Vec<_> = catalog
        .picker_changes(source.spec().id.clone(), 1, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.asset.name)
        .collect();
    assert_eq!(listed, ["n2.jpg", "n3.jpg"]);
}

#[tokio::test]
async fn pages_have_no_gaps_or_duplicates_when_many_timestamps_are_equal() {
    let catalog = Catalog::in_memory().await.unwrap();
    let source = SyntheticSource::new(503).unwrap();
    indexer::scan(&catalog, &source, false, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    loop {
        let page = catalog
            .page(None, cursor, PageSize::new(7).unwrap())
            .await
            .unwrap();
        for asset in page.items {
            assert!(seen.insert(asset.key.clone()));
            ordered.push((asset.sort_time, asset.key));
        }
        match page.next {
            None => break,
            Some(next) => cursor = Some(TimelineCursor::decode(&next.encode().unwrap()).unwrap()),
        }
    }
    assert_eq!(seen.len(), 503);
    assert!(ordered.windows(2).all(|pair| pair[0] > pair[1]));
}

#[tokio::test]
async fn failed_scan_retains_committed_pages_and_resumes_after_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("catalog.db");
    let catalog = Catalog::open(&path).await.unwrap();
    let failed = SyntheticSource::new(501).unwrap().fail_at(200);
    assert!(matches!(
        indexer::scan(&catalog, &failed, false, &CancellationToken::new(), |_| {}).await,
        Err(Error::Remote(_))
    ));
    assert_eq!(catalog.stats().await.unwrap().assets, 200);
    drop(catalog);
    let catalog = Catalog::open(path).await.unwrap();
    let resumed = SyntheticSource::new(501).unwrap();
    let mut first_progress = None;
    let completed = indexer::scan(
        &catalog,
        &resumed,
        false,
        &CancellationToken::new(),
        |progress| {
            first_progress.get_or_insert(progress.indexed);
        },
    )
    .await
    .unwrap();
    assert_eq!(first_progress, Some(300));
    assert!(completed.complete);
    assert_eq!(completed.indexed, 501);
    assert_eq!(catalog.stats().await.unwrap().assets, 501);
}

#[tokio::test]
async fn cancellation_preserves_the_last_committed_page_and_checkpoint() {
    let catalog = Catalog::in_memory().await.unwrap();
    let source = SyntheticSource::new(1000).unwrap();
    let cancel = CancellationToken::new();
    let result = indexer::scan(&catalog, &source, false, &cancel, |_| cancel.cancel()).await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(catalog.stats().await.unwrap().assets, 100);
    let checkpoint = catalog
        .begin_scan(source.spec().id.clone(), false)
        .await
        .unwrap();
    assert_eq!(checkpoint.cursor.as_deref(), Some("100"));
    assert_eq!(checkpoint.indexed, 100);
    assert!(!checkpoint.complete);
}

#[tokio::test]
async fn an_old_scan_cannot_commit_after_a_refresh_started() {
    let catalog = Catalog::in_memory().await.unwrap();
    let source = SyntheticSource::new(20).unwrap();
    catalog
        .register_source(source.spec().clone())
        .await
        .unwrap();
    let old = catalog
        .begin_scan(source.spec().id.clone(), false)
        .await
        .unwrap();
    let new = catalog
        .begin_scan(source.spec().id.clone(), true)
        .await
        .unwrap();
    let page = source
        .list_page(None, PageSize::default(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        catalog.apply_page(source.spec().clone(), old, page).await,
        Err(Error::StaleScan)
    ));
    assert_eq!(catalog.stats().await.unwrap().assets, 0);
    assert_eq!(new.generation, 2);
}

#[tokio::test]
async fn asset_identity_deduplicates_overlapping_sources_but_preserves_membership() {
    let catalog = Catalog::in_memory().await.unwrap();
    let source = SyntheticSource::new(12).unwrap();
    indexer::scan(&catalog, &source, false, &CancellationToken::new(), |_| {})
        .await
        .unwrap();
    let mut second = source.spec().clone();
    second.id = SourceId::new("second").unwrap();
    second.root = NodeId::new("another-folder").unwrap();
    catalog.register_source(second.clone()).await.unwrap();
    let checkpoint = catalog.begin_scan(second.id.clone(), false).await.unwrap();
    let page = source
        .list_page(None, PageSize::new(3).unwrap(), &CancellationToken::new())
        .await
        .unwrap();
    catalog
        .apply_page(second.clone(), checkpoint, page)
        .await
        .unwrap();
    assert_eq!(catalog.stats().await.unwrap().assets, 12);
    let page = catalog
        .page(Some(second.id.clone()), None, PageSize::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(matches!(
        catalog
            .page(None, page.next.clone(), PageSize::default())
            .await,
        Err(Error::Invalid(_))
    ));
    let last = catalog
        .page(Some(second.id), page.next, PageSize::new(2).unwrap())
        .await
        .unwrap();
    assert_eq!(last.items.len(), 1);
    assert!(last.next.is_none());
}

#[tokio::test]
async fn cross_account_page_is_rejected_without_advancing_the_checkpoint() {
    let catalog = Catalog::in_memory().await.unwrap();
    let source = SyntheticSource::new(10).unwrap();
    catalog
        .register_source(source.spec().clone())
        .await
        .unwrap();
    let checkpoint = catalog
        .begin_scan(source.spec().id.clone(), false)
        .await
        .unwrap();
    let mut page = source
        .list_page(None, PageSize::default(), &CancellationToken::new())
        .await
        .unwrap();
    page.assets[3].key.account = AccountId::new("other-account").unwrap();
    assert!(matches!(
        catalog
            .apply_page(source.spec().clone(), checkpoint.clone(), page)
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(catalog.stats().await.unwrap().assets, 0);
    assert_eq!(
        catalog
            .begin_scan(source.spec().id.clone(), false)
            .await
            .unwrap(),
        checkpoint
    );
}

#[tokio::test]
async fn newer_schema_is_refused_and_left_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("newer.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA user_version=999; CREATE TABLE future_data (value TEXT); INSERT INTO future_data VALUES ('preserve');").unwrap();
    drop(conn);
    assert!(matches!(
        Catalog::open(&path).await,
        Err(Error::NewerSchema { found: 999, .. })
    ));
    let conn = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM future_data", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "preserve"
    );
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        999
    );
}

#[tokio::test]
async fn empty_refresh_does_not_infer_deletion_without_reconciliation() {
    let catalog = Catalog::in_memory().await.unwrap();
    indexer::scan(
        &catalog,
        &SyntheticSource::new(10).unwrap(),
        false,
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    indexer::scan(
        &catalog,
        &SyntheticSource::new(0).unwrap(),
        true,
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(catalog.stats().await.unwrap().assets, 10);
}

#[test]
fn media_detection_does_not_treat_a_document_with_an_image_extension_as_a_photo() {
    assert_eq!(MediaKind::classify("application/pdf", "invoice.jpg"), None);
    assert_eq!(
        MediaKind::classify("application/octet-stream", "PHOTO.HEIC"),
        Some(MediaKind::Image)
    );
    assert_eq!(
        MediaKind::classify("VIDEO/MP4; codecs=avc1", "clip"),
        Some(MediaKind::Video)
    );
    assert!(PageSize::new(0).is_err());
    assert!(PageSize::new(251).is_err());
    assert!(AccountId::new("").is_err());
    assert!(TimelineCursor::decode(&"a".repeat(20_000)).is_err());
}

#[tokio::test]
async fn the_first_committed_page_is_queryable_while_the_remote_scan_is_still_blocked() {
    use proton_gallery_rs::source::{SourceCapabilities, SourcePage};
    use std::sync::Arc;

    struct GatedSource(SyntheticSource, Arc<tokio::sync::Semaphore>);
    #[async_trait::async_trait]
    impl MediaSource for GatedSource {
        fn spec(&self) -> &SourceSpec {
            self.0.spec()
        }
        fn capabilities(&self) -> SourceCapabilities {
            self.0.capabilities()
        }
        async fn list_page(
            &self,
            cursor: Option<&str>,
            size: PageSize,
            cancel: &CancellationToken,
        ) -> proton_gallery_rs::Result<SourcePage> {
            if cursor.is_some() {
                tokio::select! {
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    permit = self.1.acquire() => permit.unwrap().forget(),
                }
            }
            self.0.list_page(cursor, size, cancel).await
        }
    }

    let catalog = Catalog::in_memory().await.unwrap();
    let source = GatedSource(
        SyntheticSource::new(300).unwrap(),
        Arc::new(tokio::sync::Semaphore::new(0)),
    );
    let cancel = CancellationToken::new();
    let scan_catalog = catalog.clone();
    let scan_cancel = cancel.clone();
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready = Some(ready);
        indexer::scan(&scan_catalog, &source, false, &scan_cancel, |_| {
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
        })
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let page = catalog.page(None, None, PageSize::default()).await.unwrap();
    assert_eq!(page.items.len(), 100);
    assert!(!task.is_finished());
    cancel.cancel();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));
}
