//! Durable Photos feed, listing and metadata work for one timeline source.
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhotosCheckpoint {
    pub cursor: Option<String>,
    /// 1 reuses upgraded catalog records; 2 revalidates all listed records.
    pub listing: u8,
    pub replay: bool,
    pub album_listing: bool,
    pub albums_pending: bool,
    pub last_success: Option<i64>,
    pub revision: u64,
}

#[derive(Debug)]
pub enum PhotoChange {
    Updated(NodeId),
    Removed(NodeId),
}

#[derive(Debug)]
pub struct PendingPhoto {
    pub node: NodeId,
    pub capture_time: Option<i64>,
    pub attempts: u32,
}

#[derive(Debug)]
pub enum PhotoResolution {
    Unresolved,
    Ignore,
    Removed,
    Asset { asset: Box<Asset>, timeline: bool },
}

impl Catalog {
    pub async fn open_photos_sync(&self, source: SourceSpec) -> Result<PhotosCheckpoint> {
        self.register_source(source.clone()).await?;
        self.set_source_kind(source.id.clone(), crate::library::SourceKind::Photos)
            .await?;
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let prior: Option<String> = tx
                .query_row(
                    "SELECT source FROM photos_sync_active WHERE account=?1",
                    [source.account.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT OR IGNORE INTO photos_sync(source) VALUES (?1)",
                [source.id.as_str()],
            )?;
            if prior.as_deref() != Some(source.id.as_str()) {
                if let Some(prior) = prior {
                    tx.execute(
                        "UPDATE photos_sync SET revision=max(revision,
                            (SELECT revision+1 FROM photos_sync WHERE source=?2)) WHERE source=?1",
                        params![source.id.as_str(), prior],
                    )?;
                }
                tx.execute(
                    "INSERT INTO photos_sync_active(account,source) VALUES(?1,?2)
                     ON CONFLICT(account) DO UPDATE SET source=excluded.source",
                    params![source.account.as_str(), source.id.as_str()],
                )?;
            }
            let checkpoint = read_checkpoint(&tx, &source.id)?;
            tx.commit()?;
            Ok(checkpoint)
        })
        .await
    }

    pub async fn photos_checkpoint(&self, source: SourceId) -> Result<PhotosCheckpoint> {
        self.run(move |conn| read_checkpoint(conn, &source)).await
    }

    pub async fn account_photos_sync(
        &self,
        account: AccountId,
    ) -> Result<Option<(SourceId, PhotosCheckpoint)>> {
        self.run(move |conn| {
            let source: Option<String> = conn
                .query_row(
                    "SELECT source FROM photos_sync_active WHERE account=?1",
                    [account.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            source
                .map(|id| {
                    let id = SourceId::new(id)?;
                    let checkpoint = read_checkpoint(conn, &id)?;
                    Ok((id, checkpoint))
                })
                .transpose()
        })
        .await
    }

    /// Changes and the continuation are visible together, including work that
    /// still needs a network call. Process updates in feed order.
    pub async fn commit_photos_events(
        &self,
        source: SourceSpec,
        expected: Option<String>,
        next: String,
        changes: Vec<PhotoChange>,
        gap: bool,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let old = read_checkpoint(&tx, &source.id)?;
            if old.cursor != expected {
                return Err(Error::StaleScan);
            }
            let mut changed = 0;
            for change in &changes {
                match change {
                    PhotoChange::Updated(node) => {
                        tx.execute(
                            "DELETE FROM photos_removed WHERE (account,volume,node)=(?1,?2,?3)",
                            params![source.account.as_str(), source.volume.as_str(), node.as_str()],
                        )?;
                        tx.execute(
                            "INSERT INTO photos_pending(source,node) VALUES(?1,?2)
                             ON CONFLICT(source,node) DO UPDATE SET capture_time=NULL, attempts=0",
                            params![source.id.as_str(), node.as_str()],
                        )?;
                    }
                    PhotoChange::Removed(node) => {
                        changed += remove_photo(&tx, &source.account, &source.volume, node)?;
                    }
                }
            }
            let needs_listing = gap;
            tx.execute(
                "UPDATE photos_sync SET cursor=?2,listing=CASE WHEN ?3 THEN 2 ELSE listing END,
                 replay=0, albums_pending=albums_pending OR ?4, revision=revision+?5 WHERE source=?1",
                params![source.id.as_str(), next, needs_listing, needs_listing || !changes.is_empty(), changed > 0],
            )?;
            tx.commit()?;
            Ok(())
        }).await
    }

    /// A complete listing supplies the membership truth and queues metadata.
    /// Known metadata stays available while work is pending.
    pub async fn commit_photos_listing(
        &self,
        source: SourceSpec,
        expected: PhotosCheckpoint,
        listed: Vec<(NodeId, i64)>,
    ) -> Result<()> {
        if listed.len() > 2_000_000 {
            return Err(Error::Invalid("Photos listing exceeds bounds".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            if read_checkpoint(&tx, &source.id)? != expected {
                return Err(Error::StaleScan);
            }
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS photos_listed(node TEXT PRIMARY KEY, capture_time INTEGER NOT NULL);
                 DELETE FROM temp.photos_listed;",
            )?;
            {
                let mut insert = tx.prepare_cached("INSERT OR IGNORE INTO temp.photos_listed VALUES (?1,?2)")?;
                for (node, time) in listed {
                    insert.execute(params![node.as_str(), time])?;
                }
            }
            let scope = params![source.id.as_str(), source.account.as_str(), source.volume.as_str()];
            let mut changed = 0;
            if expected.listing != 0 {
                tx.execute(
                    "DELETE FROM photos_removed WHERE account=?1 AND volume=?2 AND node IN (SELECT node FROM temp.photos_listed)",
                    params![source.account.as_str(), source.volume.as_str()],
                )?;
                tx.execute(
                    "DELETE FROM photos_pending WHERE source=?1 AND capture_time IS NOT NULL AND node NOT IN (SELECT node FROM temp.photos_listed)",
                    [source.id.as_str()],
                )?;
                if expected.listing == 2 {
                    tx.execute(
                        "INSERT INTO photos_pending(source,node,capture_time)
                         SELECT ?1,node,capture_time FROM temp.photos_listed WHERE true
                         ON CONFLICT(source,node) DO UPDATE SET capture_time=excluded.capture_time,attempts=0",
                        [source.id.as_str()],
                    )?;
                } else {
                    changed += tx.execute(
                        "UPDATE assets AS a SET sort_time=l.capture_time,
                         payload=json_set(a.payload,'$.sort_time',l.capture_time)
                         FROM temp.photos_listed l JOIN memberships m ON m.node=l.node
                         WHERE (m.source,m.account,m.volume)=(?1,?2,?3)
                         AND (a.account,a.volume,a.node)=(m.account,m.volume,m.node)
                         AND a.sort_time<>l.capture_time",
                        scope,
                    )?;
                    // A v12 catalog already has decrypted records. Keep those
                    // records and enqueue only timeline members we lack.
                    tx.execute(
                        "INSERT OR IGNORE INTO photos_pending(source,node,capture_time)
                         SELECT ?1,l.node,l.capture_time FROM temp.photos_listed l
                         WHERE NOT EXISTS (SELECT 1 FROM memberships m
                             WHERE (m.source,m.account,m.volume,m.node)=(?1,?2,?3,l.node))",
                        scope,
                    )?;
                }
            }
            changed += tx.execute(
                "DELETE FROM memberships WHERE source=?1 AND node NOT IN (SELECT node FROM temp.photos_listed)",
                [source.id.as_str()],
            )?;
            tx.execute(
                "UPDATE photos_sync SET listing=0,replay=replay OR ?2,revision=revision+?3 WHERE source=?1",
                params![source.id.as_str(), expected.listing != 0, changed > 0],
            )?;
            tx.execute("DELETE FROM temp.photos_listed", [])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn queue_photos_repair(&self, source: SourceId) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE photos_sync SET listing=2,albums_pending=1 WHERE source=?1",
                [source.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn pending_photos(&self, source: SourceId, limit: u32) -> Result<Vec<PendingPhoto>> {
        self.run(move |conn| {
            let mut query = conn.prepare(
                "SELECT node,capture_time,attempts FROM photos_pending WHERE source=?1
                 ORDER BY attempts,node LIMIT ?2",
            )?;
            query
                .query_map(params![source.as_str(), limit], |r| {
                    Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?))
                })?
                .map(|row| {
                    let (node, capture_time, attempts) = row?;
                    Ok(PendingPhoto {
                        node: NodeId::new(node)?,
                        capture_time,
                        attempts,
                    })
                })
                .collect()
        })
        .await
    }

    pub async fn commit_photos_metadata(
        &self,
        source: SourceSpec,
        resolutions: Vec<(PendingPhoto, PhotoResolution)>,
    ) -> Result<u64> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut changed = 0;
            let mut indexed = 0;
            for (pending, result) in resolutions {
                let node = pending.node.as_str();
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM photos_pending WHERE source=?1 AND node=?2 AND attempts=?3)",
                    params![source.id.as_str(), node, pending.attempts], |r| r.get(0),
                )?;
                if !exists {
                    continue;
                }
                if matches!(result, PhotoResolution::Unresolved) {
                    tx.execute(
                        "UPDATE photos_pending SET attempts=attempts+1 WHERE source=?1 AND node=?2",
                        params![source.id.as_str(), node],
                    )?;
                    continue;
                }
                match result {
                    PhotoResolution::Asset {asset, timeline} => {
                        if asset.key.account != source.account || asset.key.volume != source.volume
                            || asset.key.node != pending.node {
                            return Err(Error::Invalid("Photos metadata outside its scope".into()));
                        }
                        let key = params![source.account.as_str(), source.volume.as_str(), node];
                        let removed: bool = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM photos_removed WHERE (account,volume,node)=(?1,?2,?3))",
                            key, |r| r.get(0),
                        )?;
                        if !removed {
                            changed += tx.execute(
                                "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)
                                 ON CONFLICT(account,volume,node) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload
                                 WHERE assets.sort_time<>excluded.sort_time OR assets.payload<>excluded.payload",
                                params![source.account.as_str(), source.volume.as_str(), node, asset.sort_time, serde_json::to_string(&asset)?],
                            )?;
                            if timeline {
                                changed += tx.execute(
                                    "INSERT INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)
                                     ON CONFLICT DO NOTHING",
                                    params![source.id.as_str(), source.account.as_str(), source.volume.as_str(), node],
                                )?;
                            } else {
                                changed += tx.execute(
                                    "DELETE FROM memberships WHERE (source,account,volume,node)=(?1,?2,?3,?4)",
                                    params![source.id.as_str(), source.account.as_str(), source.volume.as_str(), node],
                                )?;
                            }
                            indexed += 1;
                        }
                    }
                    PhotoResolution::Removed => {
                        changed += remove_photo(&tx, &source.account, &source.volume, &pending.node)?;
                    }
                    PhotoResolution::Ignore | PhotoResolution::Unresolved => {}
                }
                tx.execute("DELETE FROM photos_pending WHERE source=?1 AND node=?2", params![source.id.as_str(), node])?;
            }
            if changed > 0 {
                tx.execute("UPDATE photos_sync SET revision=revision+1 WHERE source=?1", [source.id.as_str()])?;
            }
            tx.commit()?;
            Ok(indexed)
        }).await
    }

    pub async fn commit_photos_album_listing(
        &self,
        source: SourceSpec,
        albums: Vec<SourceSpec>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let ids: Vec<_> = albums.iter().map(|album| album.id.as_str()).collect();
            let ids = serde_json::to_string(&ids)?;
            let removed = crate::photo_albums::retain_photo_albums_tx(&tx, &source.account, &ids)?;
            tx.execute("DELETE FROM photos_album_pending WHERE source=?1", [source.id.as_str()])?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR IGNORE INTO photos_album_pending(source,album,node) VALUES(?1,?2,?3)",
                )?;
                for album in albums {
                    if album.volume != source.volume || album.account != source.account {
                        return Err(Error::Invalid("album outside the Photos scope".into()));
                    }
                    insert.execute(params![source.id.as_str(), album.id.as_str(), album.root.as_str()])?;
                }
            }
            tx.execute(
                "UPDATE photos_sync SET albums_pending=0,revision=revision+?2 WHERE source=?1",
                params![source.id.as_str(), removed > 0],
            )?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn pending_photos_albums(
        &self,
        source: SourceId,
        limit: u32,
    ) -> Result<Vec<(NodeId, u32)>> {
        self.run(move |conn| {
            let mut query = conn.prepare(
                "SELECT node,attempts FROM photos_album_pending WHERE source=?1 ORDER BY attempts,album LIMIT ?2",
            )?;
            query.query_map(params![source.as_str(),limit], |r| Ok((r.get::<_,String>(0)?,r.get(1)?)))?
                .map(|row| { let (node,attempts) = row?; Ok((NodeId::new(node)?,attempts)) })
                .collect()
        }).await
    }

    pub async fn finish_photos_album(
        &self,
        source: SourceId,
        node: NodeId,
        done: bool,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            if done {
                tx.execute("DELETE FROM photos_album_pending WHERE source=?1 AND node=?2", params![source.as_str(),node.as_str()])?;
                tx.execute("UPDATE photos_sync SET revision=revision+1 WHERE source=?1", [source.as_str()])?;
            } else {
                tx.execute("UPDATE photos_album_pending SET attempts=attempts+1 WHERE source=?1 AND node=?2",
                    params![source.as_str(),node.as_str()])?;
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn finish_photos_sync(&self, source: SourceId, now: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "UPDATE photos_sync SET last_success=?2 WHERE source=?1 AND listing=0 AND replay=0
                 AND albums_pending=0
                 AND NOT EXISTS(SELECT 1 FROM photos_pending p WHERE p.source=photos_sync.source)
                 AND NOT EXISTS(SELECT 1 FROM photos_album_pending p WHERE p.source=photos_sync.source)",
                params![source.as_str(),now],
            )?;
            Ok(())
        }).await
    }
}

fn read_checkpoint(conn: &Connection, source: &SourceId) -> Result<PhotosCheckpoint> {
    Ok(conn.query_row(
        "SELECT cursor,listing,replay,albums_pending,last_success,revision,
         albums_pending OR EXISTS(SELECT 1 FROM photos_album_pending p WHERE p.source=photos_sync.source)
         FROM photos_sync WHERE source=?1",
        [source.as_str()], |r| Ok(PhotosCheckpoint {
            cursor:r.get(0)?, listing:r.get(1)?, replay:r.get(2)?,
            album_listing:r.get(3)?, last_success:r.get(4)?, revision:r.get::<_, i64>(5)? as u64,
            albums_pending:r.get(6)?,
        }),
    )?)
}

pub(crate) fn remove_photo(
    tx: &Transaction<'_>,
    account: &AccountId,
    volume: &VolumeId,
    node: &NodeId,
) -> Result<usize> {
    let key = params![account.as_str(), volume.as_str(), node.as_str()];
    let mut changed = tx.execute(
        "INSERT OR IGNORE INTO photos_removed VALUES (?1,?2,?3)",
        key,
    )?;
    tx.execute(
        "DELETE FROM photos_pending WHERE node=?3 AND source IN (
            SELECT id FROM sources WHERE json_extract(payload,'$.account')=?1 AND json_extract(payload,'$.volume')=?2)",
        key,
    )?;
    changed += tx.execute(
        "DELETE FROM memberships WHERE (account,volume,node)=(?1,?2,?3)",
        key,
    )?;
    changed += tx.execute(
        "DELETE FROM memberships WHERE source IN (
            SELECT id FROM sources WHERE json_extract(payload,'$.account')=?1
            AND json_extract(payload,'$.volume')=?2 AND json_extract(payload,'$.root')=?3
            AND id IN (SELECT source FROM source_kinds WHERE kind='album'))",
        key,
    )?;
    changed += tx.execute(
        "DELETE FROM album_nodes WHERE source IN (
            SELECT id FROM sources WHERE json_extract(payload,'$.account')=?1
            AND json_extract(payload,'$.volume')=?2 AND json_extract(payload,'$.root')=?3
            AND id IN (SELECT source FROM source_kinds WHERE kind='album'))",
        key,
    )?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer;
    use crate::source::MediaSource;
    use crate::synthetic::SyntheticSource;

    async fn seed(catalog: &Catalog, source: &SourceSpec, cursor: &str) -> PhotosCheckpoint {
        catalog.open_photos_sync(source.clone()).await.unwrap();
        catalog
            .commit_photos_events(source.clone(), None, cursor.into(), vec![], false)
            .await
            .unwrap();
        catalog.photos_checkpoint(source.id.clone()).await.unwrap()
    }

    #[tokio::test]
    async fn an_upgraded_large_catalog_reuses_timeline_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let original = SyntheticSource::new(13_000).unwrap();
        let source = original.spec().clone();
        let catalog = Catalog::open(&path).await.unwrap();
        indexer::scan(
            &catalog,
            &original,
            false,
            &crate::CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        // The fixture contains exactly the tables and records an upgrade from v12 sees.
        catalog.run(|conn| {
            conn.execute_batch(
                "DROP TABLE photo_locations; DROP TABLE photos_album_pending; DROP TABLE photos_sync_active; DROP TABLE photos_pending;
                 DROP TABLE photos_sync; DROP TABLE photos_removed;
                 PRAGMA user_version=12;",
            )?;
            Ok(())
        }).await.unwrap();
        drop(catalog);

        let catalog = Catalog::open(&path).await.unwrap();
        let checkpoint = seed(&catalog, &source, "before-listing").await;
        let listed = (0..13_000)
            .map(|i| {
                let asset = original.asset(i).unwrap();
                (asset.key.node, asset.sort_time)
            })
            .collect();
        catalog
            .commit_photos_listing(source.clone(), checkpoint, listed)
            .await
            .unwrap();
        assert!(
            catalog
                .photos_checkpoint(source.id.clone())
                .await
                .unwrap()
                .replay
        );

        catalog
            .commit_photos_events(
                source.clone(),
                Some("before-listing".into()),
                "after-listing".into(),
                vec![],
                false,
            )
            .await
            .unwrap();
        catalog
            .commit_photos_album_listing(source.clone(), vec![])
            .await
            .unwrap();
        catalog
            .finish_photos_sync(source.id.clone(), 1_000)
            .await
            .unwrap();
        for _ in 0..3 {
            let checkpoint = catalog.open_photos_sync(source.clone()).await.unwrap();
            assert_eq!(checkpoint.listing, 0);
            assert_eq!(checkpoint.last_success, Some(1_000));
            assert!(
                catalog
                    .pending_photos(source.id.clone(), 100)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(catalog.source_count(source.id).await.unwrap(), 13_000);
    }

    #[tokio::test]
    async fn a_failed_cursor_write_rolls_back_trash_and_restores_can_rejoin() {
        let catalog = Catalog::in_memory().await.unwrap();
        let original = SyntheticSource::new(2).unwrap();
        let source = original.spec().clone();
        indexer::scan(
            &catalog,
            &original,
            false,
            &crate::CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        let asset = original.asset(0).unwrap();
        let node = asset.key.node.clone();
        let album = SourceSpec {
            id: SourceId::new("example-album").unwrap(),
            root: NodeId::new("album-node").unwrap(),
            ..source.clone()
        };
        seed(&catalog, &source, "e1").await;
        catalog
            .sync_photo_album(album.clone(), vec![asset.clone()])
            .await
            .unwrap();
        catalog
            .run(|conn| {
                conn.execute_batch(
                    "CREATE TEMP TRIGGER fail_cursor BEFORE UPDATE OF cursor ON photos_sync
                 BEGIN SELECT RAISE(ABORT,'simulated write failure'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let changes = || vec![PhotoChange::Removed(node.clone())];
        assert!(
            catalog
                .commit_photos_events(
                    source.clone(),
                    Some("e1".into()),
                    "e2".into(),
                    changes(),
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .photos_checkpoint(source.id.clone())
                .await
                .unwrap()
                .cursor
                .as_deref(),
            Some("e1")
        );
        assert_eq!(catalog.source_count(source.id.clone()).await.unwrap(), 2);
        assert_eq!(catalog.source_count(album.id.clone()).await.unwrap(), 1);
        catalog
            .run(|conn| {
                conn.execute_batch("DROP TRIGGER fail_cursor")?;
                Ok(())
            })
            .await
            .unwrap();

        let old_listing = catalog.photos_checkpoint(source.id.clone()).await.unwrap();
        catalog
            .commit_photos_events(
                source.clone(),
                Some("e1".into()),
                "e2".into(),
                changes(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(catalog.source_count(source.id.clone()).await.unwrap(), 1);
        assert_eq!(catalog.source_count(album.id.clone()).await.unwrap(), 0);
        assert!(matches!(
            catalog
                .commit_photos_listing(
                    source.clone(),
                    old_listing,
                    vec![(node.clone(), asset.sort_time)]
                )
                .await,
            Err(Error::StaleScan)
        ));

        // A concurrent album fetch with the old membership cannot restore it.
        catalog
            .sync_photo_album(album.clone(), vec![asset.clone()])
            .await
            .unwrap();
        assert_eq!(catalog.source_count(album.id.clone()).await.unwrap(), 0);

        catalog
            .commit_photos_events(
                source.clone(),
                Some("e2".into()),
                "e3".into(),
                vec![PhotoChange::Updated(node)],
                false,
            )
            .await
            .unwrap();
        let mut pending = catalog.pending_photos(source.id.clone(), 1).await.unwrap();
        assert_eq!(pending.len(), 1);
        catalog
            .commit_photos_metadata(
                source.clone(),
                vec![(
                    pending.remove(0),
                    PhotoResolution::Asset {
                        asset: Box::new(asset.clone()),
                        timeline: true,
                    },
                )],
            )
            .await
            .unwrap();
        catalog
            .sync_photo_album(album.clone(), vec![asset])
            .await
            .unwrap();
        assert_eq!(catalog.source_count(source.id).await.unwrap(), 2);
        assert_eq!(catalog.source_count(album.id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn gap_recovery_resumes_queued_batches_and_is_scoped_to_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let original = SyntheticSource::new(5).unwrap();
        let source = original.spec().clone();
        let catalog = Catalog::open(&path).await.unwrap();
        seed(&catalog, &source, "e1").await;
        catalog
            .commit_photos_events(source.clone(), Some("e1".into()), "e2".into(), vec![], true)
            .await
            .unwrap();
        let before = catalog.photos_checkpoint(source.id.clone()).await.unwrap();
        assert_eq!(before.listing, 2);
        let listed = (0..5)
            .map(|i| {
                let asset = original.asset(i).unwrap();
                (asset.key.node, asset.sort_time)
            })
            .collect();
        catalog
            .commit_photos_listing(source.clone(), before, listed)
            .await
            .unwrap();
        catalog
            .commit_photos_events(
                source.clone(),
                Some("e2".into()),
                "e3".into(),
                vec![],
                false,
            )
            .await
            .unwrap();
        let mut pending = catalog
            .pending_photos(source.id.clone(), 2)
            .await
            .unwrap()
            .into_iter();
        let resolved = pending.next().unwrap();
        let resolved_node = resolved.node.clone();
        let asset = (0..5)
            .map(|i| original.asset(i).unwrap())
            .find(|asset| asset.key.node == resolved_node)
            .unwrap();
        let skipped = pending.next().unwrap();
        let skipped_node = skipped.node.clone();
        catalog
            .commit_photos_metadata(
                source.clone(),
                vec![
                    (
                        resolved,
                        PhotoResolution::Asset {
                            asset: Box::new(asset),
                            timeline: true,
                        },
                    ),
                    (skipped, PhotoResolution::Unresolved),
                ],
            )
            .await
            .unwrap();
        drop(catalog);

        let catalog = Catalog::open(&path).await.unwrap();
        let checkpoint = catalog.photos_checkpoint(source.id.clone()).await.unwrap();
        assert_eq!(checkpoint.cursor.as_deref(), Some("e3"));
        assert_eq!(checkpoint.listing, 0);
        let remaining = catalog.pending_photos(source.id.clone(), 10).await.unwrap();
        assert_eq!(remaining.len(), 4);
        assert!(
            !remaining
                .iter()
                .any(|pending| pending.node == resolved_node)
        );
        assert_eq!(remaining.last().unwrap().node, skipped_node);
        assert_eq!(remaining.last().unwrap().attempts, 1);

        assert!(
            catalog
                .account_photos_sync(AccountId::new("different-account").unwrap())
                .await
                .unwrap()
                .is_none()
        );
        catalog
            .forget_account(source.account.clone())
            .await
            .unwrap();
        assert!(
            catalog
                .account_photos_sync(source.account)
                .await
                .unwrap()
                .is_none()
        );
    }
}
