//! Proton Photos albums in the catalog.
//!
//! Each album is a source of kind 'album' and a one-level gallery in
//! `album_nodes` (gallery and source are the album itself), so album lists,
//! presentation settings, covers and paging reuse the folder-album code.
//! An album's photos are memberships; the assets are shared with the
//! timeline, and photos that live only in an album are added as assets too.
use rusqlite::{Transaction, params};

use crate::catalog::Catalog;
use crate::library::{Album, SourceKind};
use crate::model::*;
use crate::{Error, Result};

impl Catalog {
    /// Record one album as Proton lists it now: its name and every photo.
    /// Photos no longer in it leave it.
    pub async fn sync_photo_album(&self, album: SourceSpec, assets: Vec<Asset>) -> Result<()> {
        if assets.len() > 200_000 {
            return Err(Error::Invalid("album exceeds bounds".into()));
        }
        if assets
            .iter()
            .any(|a| a.key.account != album.account || a.name.len() > 16_384 || a.mime.len() > 1024)
        {
            return Err(Error::Invalid(
                "album item outside the album's account".into(),
            ));
        }
        self.register_source(album.clone()).await?;
        self.set_source_kind(album.id.clone(), SourceKind::Album)
            .await?;
        let payloads = assets
            .iter()
            .map(serde_json::to_string)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let album_removed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM photos_removed WHERE (account,volume,node)=(?1,?2,?3))",
                params![album.account.as_str(),album.volume.as_str(),album.root.as_str()], |r| r.get(0),
            )?;
            if album_removed {
                return Ok(());
            }
            tx.execute(
                "INSERT OR IGNORE INTO album_nodes(gallery,source,parent) VALUES (?1,?1,NULL)",
                [album.id.as_str()],
            )?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS album_listed(account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
                    PRIMARY KEY(account,volume,node));
                 DELETE FROM temp.album_listed;",
            )?;
            {
                let mut asset = tx.prepare_cached(
                    "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(account,volume,node) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload",
                )?;
                let mut member = tx.prepare_cached(
                    "INSERT OR IGNORE INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)",
                )?;
                let mut listed = tx.prepare_cached(
                    "INSERT OR IGNORE INTO temp.album_listed(account,volume,node) VALUES (?1,?2,?3)",
                )?;
                let mut removed = tx.prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM photos_removed WHERE (account,volume,node)=(?1,?2,?3))",
                )?;
                for (item, payload) in assets.iter().zip(&payloads) {
                    let key = &item.key;
                    let (account, volume, node) = (key.account.as_str(), key.volume.as_str(), key.node.as_str());
                    if removed.query_row(params![account,volume,node], |r| r.get::<_,bool>(0))? {
                        continue;
                    }
                    asset.execute(params![account, volume, node, item.sort_time, payload])?;
                    member.execute(params![album.id.as_str(), account, volume, node])?;
                    listed.execute(params![account, volume, node])?;
                }
            }
            tx.execute(
                "DELETE FROM memberships WHERE source=?1 AND NOT EXISTS(SELECT 1 FROM temp.album_listed l
                    WHERE (l.account,l.volume,l.node)=(memberships.account,memberships.volume,memberships.node))",
                [album.id.as_str()],
            )?;
            tx.execute("DELETE FROM temp.album_listed", [])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// After listing every album of `account`: forget the albums not in `keep`.
    pub async fn retain_photo_albums(
        &self,
        account: AccountId,
        keep: Vec<SourceId>,
    ) -> Result<u64> {
        let keep = serde_json::to_string(&keep.iter().map(SourceId::as_str).collect::<Vec<_>>())?;
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let removed = retain_photo_albums_tx(&tx, &account, &keep)?;
            tx.commit()?;
            Ok(removed)
        })
        .await
    }

    /// The Proton Photos albums of `account`, pinned first, then by name.
    pub async fn photo_albums(&self, account: AccountId) -> Result<Vec<Album>> {
        self.query_albums(
            "n.parent IS NULL AND ?1 IS NULL AND ?2 IS NULL
             AND EXISTS(SELECT 1 FROM source_kinds k WHERE k.source=n.source AND k.kind='album')",
            vec![rusqlite::types::Value::Null, rusqlite::types::Value::Null],
            Some(account),
        )
        .await
    }

    /// The Proton album behind `source`, if it is one of `account`'s.
    pub async fn photo_album(
        &self,
        account: AccountId,
        source: SourceId,
    ) -> Result<Option<SourceSpec>> {
        self.run(move |conn| {
            use rusqlite::OptionalExtension;
            let payload: Option<String> = conn
                .query_row(
                    "SELECT s.payload FROM sources s JOIN source_kinds k ON k.source=s.id
                     WHERE s.id=?1 AND k.kind='album' AND json_extract(s.payload,'$.account')=?2",
                    params![source.as_str(), account.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            payload.map(|p| Ok(serde_json::from_str(&p)?)).transpose()
        })
        .await
    }

    /// Put photos the catalog already knows into an album right away; the
    /// next sync confirms them. Returns how many were added.
    pub async fn add_album_members(&self, album: SourceId, keys: Vec<AssetKey>) -> Result<u64> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut added = 0;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR IGNORE INTO memberships(source,account,volume,node)
                     SELECT ?1,account,volume,node FROM assets WHERE (account,volume,node)=(?2,?3,?4)",
                )?;
                for key in &keys {
                    added += insert.execute(params![album.as_str(), key.account.as_str(), key.volume.as_str(), key.node.as_str()])?;
                }
            }
            tx.commit()?;
            Ok(added as u64)
        })
        .await
    }

    /// Take photos out of an album; they stay in the timeline.
    pub async fn remove_album_members(&self, album: SourceId, keys: Vec<AssetKey>) -> Result<u64> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut removed = 0;
            {
                let mut delete = tx.prepare_cached(
                    "DELETE FROM memberships WHERE source=?1 AND (account,volume,node)=(?2,?3,?4)",
                )?;
                for key in &keys {
                    removed += delete.execute(params![
                        album.as_str(),
                        key.account.as_str(),
                        key.volume.as_str(),
                        key.node.as_str()
                    ])?;
                }
            }
            tx.commit()?;
            Ok(removed as u64)
        })
        .await
    }

    /// The catalog's record of each key, where it has one.
    pub async fn assets_by_key(&self, keys: Vec<AssetKey>) -> Result<Vec<Option<Asset>>> {
        if keys.len() > 200_000 {
            return Err(Error::Invalid("too many items requested".into()));
        }
        self.run(move |conn| {
            use rusqlite::OptionalExtension;
            let mut query = conn.prepare_cached(
                "SELECT payload FROM assets WHERE (account,volume,node)=(?1,?2,?3)",
            )?;
            keys.iter()
                .map(|key| {
                    let payload: Option<String> = query
                        .query_row(
                            params![key.account.as_str(), key.volume.as_str(), key.node.as_str()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    payload.map(|p| Ok(serde_json::from_str(&p)?)).transpose()
                })
                .collect()
        })
        .await
    }

    /// Forget one album at once, after it was deleted in Proton.
    pub async fn forget_photo_album(&self, album: SourceId) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM memberships WHERE source=?1", [album.as_str()])?;
            tx.execute("DELETE FROM album_nodes WHERE gallery=?1", [album.as_str()])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

pub(crate) fn retain_photo_albums_tx(
    tx: &Transaction<'_>,
    account: &AccountId,
    keep: &str,
) -> Result<u64> {
    let gone: Vec<String> = tx
                .prepare(
                    "SELECT k.source FROM source_kinds k JOIN sources s ON s.id=k.source
                     JOIN album_nodes n ON n.source=k.source WHERE k.kind='album' AND json_extract(s.payload,'$.account')=?1
                     AND k.source NOT IN (SELECT value FROM json_each(?2))",
                )?
                .query_map(params![account.as_str(), keep], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
    for source in &gone {
        tx.execute("DELETE FROM memberships WHERE source=?1", [source])?;
        tx.execute("DELETE FROM album_nodes WHERE gallery=?1", [source])?;
    }
    Ok(gone.len() as u64)
}
