use std::sync::Arc;
use std::time::Duration;

use proton_gallery_rs::bytes::{
    MAX_BLOCK_BYTES, MAX_READ_BYTES, ObjectStore, RangeReader, RevisionDescriptor,
};
use proton_gallery_rs::catalog::Catalog;
use proton_gallery_rs::model::RevisionId;
use proton_gallery_rs::synthetic::{SyntheticBlocks, SyntheticSource};
use proton_gallery_rs::{CancellationToken, Error};

fn descriptor(revision: &str, sizes: Vec<u64>) -> RevisionDescriptor {
    RevisionDescriptor::new(
        SyntheticSource::new(1).unwrap().asset(0).unwrap().key,
        RevisionId::new(revision).unwrap(),
        sizes,
    )
    .unwrap()
}

#[tokio::test]
async fn exact_ranges_match_a_byte_oracle_across_irregular_block_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let descriptor = descriptor("r1", vec![17, 41, 3, 29]);
    let remote = Arc::new(SyntheticBlocks::new(descriptor.clone()));
    let reader = RangeReader::new(descriptor, store, Some(remote.clone())).unwrap();
    for offset in 0..95 {
        for length in [0, 1, 3, 17, 93] {
            let bytes = reader
                .read_at(offset, length, &CancellationToken::new())
                .await
                .unwrap();
            let expected = length.min(90u64.saturating_sub(offset)) as usize;
            assert_eq!(bytes, SyntheticBlocks::expected(offset, expected));
        }
    }
    assert_eq!(
        remote.calls(),
        4,
        "each block is fetched once, despite overlapping ranges"
    );
    assert!(matches!(
        reader
            .read_at(0, MAX_READ_BYTES + 1, &CancellationToken::new())
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(
        reader
            .read_at(u64::MAX, 5, &CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn seeking_fetches_only_intersecting_blocks_and_does_not_imply_full_offline_availability() {
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let descriptor = descriptor("r1", vec![101, 211, 37, 53]);
    let remote = Arc::new(SyntheticBlocks::new(descriptor.clone()));
    let reader = RangeReader::new(descriptor.clone(), store.clone(), Some(remote.clone())).unwrap();
    assert_eq!(
        reader
            .read_at(300, 30, &CancellationToken::new())
            .await
            .unwrap(),
        SyntheticBlocks::expected(300, 30)
    );
    assert_eq!(remote.calls(), 2);
    let offline = RangeReader::new(descriptor, store, None).unwrap();
    assert_eq!(
        offline
            .read_at(300, 30, &CancellationToken::new())
            .await
            .unwrap(),
        SyntheticBlocks::expected(300, 30)
    );
    assert!(matches!(
        offline.read_at(0, 1, &CancellationToken::new()).await,
        Err(Error::OfflineMiss)
    ));
}

#[tokio::test]
async fn interrupted_pin_reuses_committed_blocks_and_reopens_without_a_remote_source() {
    let directory = tempfile::tempdir().unwrap();
    let catalog_path = directory.path().join("catalog.db");
    let store_path = directory.path().join("blocks");
    let catalog = Catalog::open(&catalog_path).await.unwrap();
    let desc = descriptor("r1", vec![17, 41, 3, 29]);
    let remote = Arc::new(SyntheticBlocks::new(desc.clone()));
    let store = ObjectStore::open(&store_path, [7; 32]).await.unwrap();
    let reader = RangeReader::new(desc.clone(), store, Some(remote.clone())).unwrap();
    let stop = CancellationToken::new();
    assert!(matches!(
        reader
            .keep_offline(&catalog, &stop, |_, _| stop.cancel())
            .await,
        Err(Error::Cancelled)
    ));
    assert_eq!(remote.calls(), 1);
    assert!(catalog.retained().await.unwrap().is_empty());
    drop(reader);
    drop(catalog);
    let catalog = Catalog::open(&catalog_path).await.unwrap();
    let store = ObjectStore::open(&store_path, [7; 32]).await.unwrap();
    let reader = RangeReader::new(desc.clone(), store.clone(), Some(remote.clone())).unwrap();
    reader
        .keep_offline(&catalog, &CancellationToken::new(), |_, _| {})
        .await
        .unwrap();
    assert_eq!(
        remote.calls(),
        4,
        "resume must not refetch the first committed block"
    );
    drop(reader);
    let stored = catalog.retained().await.unwrap();
    assert_eq!(stored, [desc]);
    let offline = RangeReader::new(stored[0].clone(), store, None).unwrap();
    assert_eq!(
        offline
            .read_at(0, 90, &CancellationToken::new())
            .await
            .unwrap(),
        SyntheticBlocks::expected(0, 90)
    );
    assert_eq!(remote.calls(), 4);
}

#[tokio::test]
async fn concurrent_consumers_share_one_fetch_even_if_their_ranges_differ() {
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let desc = descriptor("r1", vec![101]);
    let remote = Arc::new(SyntheticBlocks::new(desc.clone()).with_delay(Duration::from_millis(10)));
    let reader = RangeReader::new(desc, store, Some(remote.clone())).unwrap();
    let cancel = CancellationToken::new();
    let (a, b, c) = tokio::join!(
        reader.read_at(0, 30, &cancel),
        reader.read_at(29, 50, &cancel),
        reader.read_at(80, 21, &cancel)
    );
    assert_eq!(a.unwrap(), SyntheticBlocks::expected(0, 30));
    assert_eq!(b.unwrap(), SyntheticBlocks::expected(29, 50));
    assert_eq!(c.unwrap(), SyntheticBlocks::expected(80, 21));
    assert_eq!(remote.calls(), 1);
}

fn object_file(root: &std::path::Path) -> std::path::PathBuf {
    let prefix = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    std::fs::read_dir(prefix)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

#[tokio::test]
async fn equal_length_corruption_is_detected_offline_and_repaired_online() {
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let desc = descriptor("r1", vec![101]);
    let remote = Arc::new(SyntheticBlocks::new(desc.clone()));
    let reader = RangeReader::new(desc.clone(), store.clone(), Some(remote.clone())).unwrap();
    let cancel = CancellationToken::new();
    let expected = reader.read_at(0, 101, &cancel).await.unwrap();
    let object = object_file(directory.path());
    let mut encrypted = std::fs::read(&object).unwrap();
    assert!(
        !encrypted
            .windows(expected.len())
            .any(|bytes| bytes == expected)
    );
    *encrypted.last_mut().unwrap() ^= 1;
    std::fs::write(object, &encrypted).unwrap();
    let offline = RangeReader::new(desc, store, None).unwrap();
    assert!(matches!(
        offline.read_at(0, 101, &cancel).await,
        Err(Error::CorruptObject)
    ));
    assert_eq!(reader.read_at(0, 101, &cancel).await.unwrap(), expected);
    assert_eq!(remote.calls(), 2);
    assert_eq!(offline.read_at(0, 101, &cancel).await.unwrap(), expected);
}

#[tokio::test]
async fn another_key_cannot_read_the_same_objects_and_revisions_do_not_share_paths() {
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let old = descriptor("r1", vec![101]);
    let remote = Arc::new(SyntheticBlocks::new(old.clone()));
    let reader = RangeReader::new(old.clone(), store.clone(), Some(remote)).unwrap();
    let cancel = CancellationToken::new();
    reader.read_at(0, 10, &cancel).await.unwrap();
    assert!(matches!(
        ObjectStore::open(directory.path(), [8; 32]).await,
        Err(Error::CorruptObject)
    ));
    let new = descriptor("r2", vec![101]);
    let new_offline = RangeReader::new(new.clone(), store.clone(), None).unwrap();
    assert!(matches!(
        new_offline.read_at(0, 1, &cancel).await,
        Err(Error::OfflineMiss)
    ));
    let wrong_source = Arc::new(SyntheticBlocks::new(new));
    assert!(matches!(
        RangeReader::new(old, store, Some(wrong_source)),
        Err(Error::RevisionMismatch)
    ));
}

#[test]
fn invalid_block_maps_are_refused_including_when_deserialized() {
    let asset = SyntheticSource::new(1).unwrap().asset(0).unwrap().key;
    assert!(
        RevisionDescriptor::new(asset.clone(), RevisionId::new("r1").unwrap(), vec![0]).is_err()
    );
    assert!(
        RevisionDescriptor::new(
            asset,
            RevisionId::new("r1").unwrap(),
            vec![MAX_BLOCK_BYTES + 1]
        )
        .is_err()
    );
    let mut value = serde_json::to_value(descriptor("r1", vec![17])).unwrap();
    value["block_sizes"] = serde_json::json!([0]);
    assert!(serde_json::from_value::<RevisionDescriptor>(value).is_err());
}

#[tokio::test]
async fn a_failed_cache_write_allows_viewing_but_cannot_complete_an_offline_download() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("objects");
    let store = ObjectStore::open(&root, [7; 32]).await.unwrap();
    // Replace the empty store directory with a file to inject a write failure.
    std::fs::rename(&root, directory.path().join("original-directory")).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();
    let desc = descriptor("r1", vec![101]);
    let remote = Arc::new(SyntheticBlocks::new(desc.clone()));
    let reader = RangeReader::new(desc, store, Some(remote)).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(
        reader.read_at(0, 30, &cancel).await.unwrap(),
        SyntheticBlocks::expected(0, 30)
    );
    let catalog = Catalog::in_memory().await.unwrap();
    assert!(matches!(
        reader.keep_offline(&catalog, &cancel, |_, _| {}).await,
        Err(Error::Io(_))
    ));
    assert!(catalog.retained().await.unwrap().is_empty());
}

#[tokio::test]
async fn cancelling_the_fetching_consumer_does_not_strand_another_waiter() {
    use proton_gallery_rs::source::BlockSource;

    struct GatedBlocks {
        descriptor: RevisionDescriptor,
        started: tokio::sync::Notify,
        release: tokio::sync::Semaphore,
    }
    #[async_trait::async_trait]
    impl BlockSource for GatedBlocks {
        fn descriptor(&self) -> &RevisionDescriptor {
            &self.descriptor
        }
        async fn read_block(
            &self,
            _: usize,
            cancel: &CancellationToken,
        ) -> proton_gallery_rs::Result<Vec<u8>> {
            self.started.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                permit = self.release.acquire() => permit.unwrap().forget(),
            }
            Ok(SyntheticBlocks::expected(0, 101))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let store = ObjectStore::open(directory.path(), [7; 32]).await.unwrap();
    let desc = descriptor("r1", vec![101]);
    let source = Arc::new(GatedBlocks {
        descriptor: desc.clone(),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let reader = RangeReader::new(desc, store, Some(source.clone())).unwrap();
    let first_reader = reader.clone();
    let first_cancel = CancellationToken::new();
    let token = first_cancel.clone();
    let first = tokio::spawn(async move { first_reader.read_at(0, 20, &token).await });
    source.started.notified().await;
    let second =
        tokio::spawn(async move { reader.read_at(0, 30, &CancellationToken::new()).await });
    first_cancel.cancel();
    assert!(matches!(first.await.unwrap(), Err(Error::Cancelled)));
    tokio::time::timeout(Duration::from_secs(5), source.started.notified())
        .await
        .unwrap();
    source.release.add_permits(1);
    let bytes = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(bytes, SyntheticBlocks::expected(0, 30));
}
