use proton_gallery_rs::{catalog::Catalog, local::LocalLibrary, model::*};

#[tokio::test]
async fn imports_are_durable_deduplicated_and_confined_to_staging() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::open(dir.path().join("catalog.db")).await.unwrap();
    let local = LocalLibrary::open(dir.path().join("device"), catalog.clone())
        .await
        .unwrap();
    let outside = dir.path().join("outside");
    std::fs::write(&outside, b"fixture").unwrap();
    assert!(
        local
            .import_staged(outside.clone(), "x.jpg".into(), "image/jpeg".into(), 1)
            .await
            .is_err()
    );
    assert!(outside.exists());
    let staged = local.incoming().join("selected");
    std::fs::write(&staged, b"fixture").unwrap();
    let key = local
        .import_staged(staged.clone(), "first.jpg".into(), "image/jpeg".into(), 10)
        .await
        .unwrap();
    assert!(!staged.exists());
    let published = local.file_for(key.clone()).await.unwrap().unwrap();
    assert_eq!(std::fs::read(&published).unwrap(), b"fixture");
    // A repeated import repairs damaged bytes without duplicating or renaming metadata.
    std::fs::write(&published, b"broken").unwrap();
    std::fs::write(&staged, b"fixture").unwrap();
    assert_eq!(
        key,
        local
            .import_staged(staged, "second.jpg".into(), "image/jpeg".into(), 20)
            .await
            .unwrap()
    );
    assert_eq!(catalog.local_totals().await.unwrap(), (1, 7));
    assert_eq!(
        catalog
            .page(None, None, PageSize::default())
            .await
            .unwrap()
            .items[0]
            .name,
        "first.jpg"
    );
    assert_eq!(std::fs::read(published).unwrap(), b"fixture");
    assert_eq!(catalog.stats().await.unwrap().retained_revisions, 0);
}

#[tokio::test]
async fn empty_or_nonmedia_imports_never_publish_catalog_rows() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = Catalog::in_memory().await.unwrap();
    let local = LocalLibrary::open(dir.path(), catalog.clone())
        .await
        .unwrap();
    let staged = local.incoming().join("empty");
    std::fs::write(&staged, b"").unwrap();
    assert!(
        local
            .import_staged(staged.clone(), "empty.jpg".into(), "image/jpeg".into(), 1)
            .await
            .is_err()
    );
    std::fs::write(&staged, b"PDF").unwrap();
    assert!(
        local
            .import_staged(staged, "x.jpg".into(), "application/pdf".into(), 1)
            .await
            .is_err()
    );
    assert_eq!(catalog.stats().await.unwrap().assets, 0);
}
