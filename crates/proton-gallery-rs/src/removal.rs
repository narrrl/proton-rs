//! Media leaving the catalog: trashed from this app, or gone from Proton.
//!
//! Only memberships are removed. The asset row stays while anything else
//! refers to it (another source, a phone backup link, the picker's change
//! log), which keeps every foreign key intact; it simply stops appearing.
use rusqlite::params;

use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};

impl Catalog {
    /// Take `keys` out of every source of `account` that lists them.
    /// Returns how many memberships were removed.
    pub async fn remove_assets(&self, account: AccountId, keys: Vec<AssetKey>) -> Result<u64> {
        if keys.len() > 10_000 {
            return Err(Error::Invalid("too many items to remove at once".into()));
        }
        if keys.iter().any(|key| key.account != account) {
            return Err(Error::Invalid("item belongs to another account".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut removed = 0;
            for key in keys {
                let photos: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM photos_sync p JOIN sources s ON s.id=p.source
                       WHERE json_extract(s.payload,'$.account')=?1 AND json_extract(s.payload,'$.volume')=?2)",
                    params![key.account.as_str(), key.volume.as_str()], |r| r.get(0),
                )?;
                if photos {
                    removed += crate::photos_sync::remove_photo(&tx, &key.account, &key.volume, &key.node)?;
                    tx.execute(
                        "UPDATE photos_sync SET revision=revision+1 WHERE source IN (
                         SELECT id FROM sources WHERE json_extract(payload,'$.account')=?1 AND json_extract(payload,'$.volume')=?2)",
                        params![key.account.as_str(), key.volume.as_str()],
                    )?;
                } else {
                    removed += tx.execute(
                        "DELETE FROM memberships WHERE (account,volume,node)=(?1,?2,?3)",
                        params![key.account.as_str(),key.volume.as_str(),key.node.as_str()],
                    )?;
                }
            }
            tx.commit()?;
            Ok(removed as u64)
        })
        .await
    }

    /// After a complete listing of `source`: remove members it no longer
    /// lists. Returns how many were removed.
    pub async fn prune_source(&self, source: SourceId, listed: Vec<NodeId>) -> Result<u64> {
        if listed.len() > 2_000_000 {
            return Err(Error::Invalid("listing exceeds bounds".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS listed(node TEXT PRIMARY KEY NOT NULL);
                 DELETE FROM temp.listed;",
            )?;
            {
                let mut insert =
                    tx.prepare_cached("INSERT OR IGNORE INTO temp.listed(node) VALUES (?1)")?;
                for node in &listed {
                    insert.execute([node.as_str()])?;
                }
            }
            let removed = tx.execute(
                "DELETE FROM memberships WHERE source=?1 AND node NOT IN (SELECT node FROM temp.listed)",
                [source.as_str()],
            )?;
            tx.execute("DELETE FROM temp.listed", [])?;
            tx.commit()?;
            Ok(removed as u64)
        })
        .await
    }
}

impl Catalog {
    /// Forget everything the catalog holds of `account`: its sources,
    /// albums and their settings, media records, shapes, backup state and
    /// picker history. Decrypted names and album titles must not outlive a
    /// sign-out. The phone's own media and the app's device library stay.
    /// The freed pages are then rewritten, so the old rows are not left in
    /// the file either.
    pub async fn forget_account(&self, account: AccountId) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let account = account.as_str();
            tx.execute(
                "CREATE TEMP TABLE IF NOT EXISTS forgotten_sources(id TEXT PRIMARY KEY)",
                [],
            )?;
            tx.execute("DELETE FROM forgotten_sources", [])?;
            tx.execute(
                "INSERT INTO forgotten_sources SELECT id FROM sources WHERE json_extract(payload,'$.account')=?1",
                [account],
            )?;
            for sql in [
                "DELETE FROM album_tags WHERE source IN forgotten_sources",
                "DELETE FROM album_settings WHERE source IN forgotten_sources",
                "DELETE FROM scans WHERE source IN forgotten_sources",
                "DELETE FROM album_nodes WHERE source IN forgotten_sources OR gallery IN forgotten_sources",
                "DELETE FROM album_adds WHERE account=?1 OR album IN forgotten_sources",
                "DELETE FROM backup_albums WHERE account=?1 OR album IN forgotten_sources",
                "DELETE FROM memberships WHERE account=?1 OR source IN forgotten_sources",
                "DELETE FROM source_kinds WHERE source IN forgotten_sources",
                "DELETE FROM media_shapes WHERE account=?1",
                "DELETE FROM local_media WHERE account=?1",
                "DELETE FROM phone_backup WHERE account=?1",
                "DELETE FROM assets WHERE account=?1",
                "DELETE FROM sources WHERE id IN forgotten_sources",
                "DELETE FROM picker_changes WHERE account=?1",
                "DELETE FROM gallery_configuration WHERE account=?1",
                "DELETE FROM gallery_configuration_imported WHERE account=?1",
                "DELETE FROM gallery_visibility_verified WHERE account=?1",
                "DELETE FROM gallery_event_cursors WHERE account=?1",
                "DELETE FROM photos_removed WHERE account=?1",
            ] {
                if sql.contains("?1") {
                    tx.execute(sql, [account])?;
                } else {
                    tx.execute(sql, [])?;
                }
            }
            tx.execute("DROP TABLE forgotten_sources", [])?;
            // The system photo picker must drop its copy and start again.
            tx.execute(
                "UPDATE picker_state SET collection=lower(hex(randomblob(16))),generation=generation+1",
                [],
            )?;
            tx.commit()?;
            conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
        .await
    }
}
