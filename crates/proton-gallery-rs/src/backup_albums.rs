//! Backups into Proton albums: per phone folder, or every backup.
//!
//! Proton's upload has no album, so a photo is queued in `album_adds` once
//! it is backed up and added to its albums in batches afterwards (schema 11).
//! Media asked into an album before it is backed up (hiding a phone-only
//! photo, say) waits in the same queue for its upload.
use rusqlite::params;

use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};

/// One album that backups go into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupAlbum {
    /// The phone folder, or `None` for every backup.
    pub folder: Option<String>,
    pub album: SourceId,
}

/// A backed-up phone item waiting to be added to an album.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlbumAdd {
    pub phone: String,
    pub album: SourceId,
    pub remote: AssetKey,
}

/// The most queued adds read at once.
pub const ALBUM_ADDS_MAX: u32 = 1000;

// An album that is still in Proton: deleted albums lose their tree.
const LIVE_ALBUM: &str =
    "EXISTS(SELECT 1 FROM source_kinds k WHERE k.source=x.album AND k.kind='album')
    AND EXISTS(SELECT 1 FROM album_nodes n WHERE n.source=x.album)";

impl Catalog {
    /// Back up `folder` (or everything, for `None`) into `album` too, or stop
    /// when `add` is false. A folder may go into several albums.
    pub async fn set_backup_album(
        &self,
        account: AccountId,
        folder: Option<String>,
        album: SourceId,
        add: bool,
    ) -> Result<()> {
        let folder = folder.unwrap_or_default();
        if folder.len() > 512 {
            return Err(Error::Invalid("folder identifier too long".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            if add {
                let album_of_account: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sources s JOIN source_kinds k ON k.source=s.id
                     WHERE s.id=?1 AND k.kind='album' AND json_extract(s.payload,'$.account')=?2)",
                    params![album.as_str(), account.as_str()],
                    |r| r.get(0),
                )?;
                if !album_of_account {
                    return Err(Error::NotFound);
                }
                tx.execute(
                    "INSERT OR IGNORE INTO backup_albums(account,folder,album) VALUES (?1,?2,?3)",
                    params![account.as_str(), folder, album.as_str()],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM backup_albums WHERE account=?1 AND folder=?2 AND album=?3",
                    params![account.as_str(), folder, album.as_str()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Where backups go, for albums still in Proton. Mappings to deleted
    /// albums are dropped.
    pub async fn backup_albums(&self, account: AccountId) -> Result<Vec<BackupAlbum>> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                &format!("DELETE FROM backup_albums AS x WHERE account=?1 AND NOT ({LIVE_ALBUM})"),
                [account.as_str()],
            )?;
            let found = {
                let mut query = tx.prepare(
                    "SELECT folder,album FROM backup_albums WHERE account=?1 ORDER BY folder,album",
                )?;
                let rows = query.query_map([account.as_str()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                rows.map(|row| {
                    let (folder, album) = row?;
                    Ok(BackupAlbum {
                        folder: (!folder.is_empty()).then_some(folder),
                        album: SourceId::new(album)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?
            };
            tx.commit()?;
            Ok(found)
        })
        .await
    }

    /// Queue `phones` for `album`. Returns how many were not queued yet.
    pub async fn queue_album_adds(
        &self,
        account: AccountId,
        phones: Vec<String>,
        album: SourceId,
    ) -> Result<u64> {
        if phones.len() > 100_000 {
            return Err(Error::Invalid("too many items".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let mut queued = 0;
            {
                let mut insert = tx.prepare(
                    "INSERT OR IGNORE INTO album_adds(account,node,album)
                     SELECT ?1,node,?3 FROM phone_media WHERE node=?2",
                )?;
                for phone in &phones {
                    queued += insert.execute(params![account.as_str(), phone, album.as_str()])?;
                }
            }
            tx.commit()?;
            Ok(queued as u64)
        })
        .await
    }

    /// Queue a phone item that was just backed up for the albums its folder
    /// and every backup go into.
    pub async fn queue_backup_albums(&self, account: AccountId, phone: String) -> Result<u64> {
        self.run(move |conn| {
            Ok(conn.execute(
                "INSERT OR IGNORE INTO album_adds(account,node,album)
                 SELECT ?1,p.node,x.album FROM phone_media p JOIN backup_albums x
                    ON x.account=?1 AND (x.folder=p.folder OR x.folder='')
                 WHERE p.node=?2",
                params![account.as_str(), phone],
            )? as u64)
        })
        .await
    }

    /// Queue every backed-up item of `folder` (of every backup folder, for
    /// `None`) for `album`: photos backed up before the album was chosen.
    pub async fn queue_backed_up(
        &self,
        account: AccountId,
        folder: Option<String>,
        album: SourceId,
    ) -> Result<u64> {
        self.run(move |conn| {
            Ok(conn.execute(
                "INSERT OR IGNORE INTO album_adds(account,node,album)
                 SELECT ?1,p.node,?3 FROM phone_media p JOIN phone_backup b ON b.account=?1 AND b.node=p.node
                 WHERE b.state='backed_up'
                    AND (p.folder=?2 OR (?2 IS NULL AND p.folder IN (SELECT folder FROM backup_folders)))",
                params![account.as_str(), folder, album.as_str()],
            )? as u64)
        })
        .await
    }

    /// Queued adds whose media is backed up, by album. Adds for deleted
    /// albums are dropped.
    pub async fn album_adds(&self, account: AccountId, limit: u32) -> Result<Vec<AlbumAdd>> {
        let limit = limit.clamp(1, ALBUM_ADDS_MAX);
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                &format!("DELETE FROM album_adds AS x WHERE account=?1 AND NOT ({LIVE_ALBUM})"),
                [account.as_str()],
            )?;
            let adds = {
                let mut query = tx.prepare(
                    "SELECT x.node,x.album,b.remote_volume,b.remote_node FROM album_adds x
                     JOIN phone_backup b ON b.account=x.account AND b.node=x.node AND b.state='backed_up'
                     WHERE x.account=?1 ORDER BY x.album,x.node LIMIT ?2",
                )?;
                let rows = query.query_map(params![account.as_str(), limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?;
                rows.map(|row| {
                    let (phone, album, volume, node) = row?;
                    Ok(AlbumAdd {
                        phone,
                        album: SourceId::new(album)?,
                        remote: AssetKey {
                            account: account.clone(),
                            volume: VolumeId::new(volume)?,
                            node: NodeId::new(node)?,
                        },
                    })
                })
                .collect::<Result<Vec<_>>>()?
            };
            tx.commit()?;
            Ok(adds)
        })
        .await
    }

    /// Forget queued adds that are done (or can never be done).
    pub async fn finish_album_adds(
        &self,
        account: AccountId,
        done: Vec<(String, SourceId)>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut delete =
                    tx.prepare("DELETE FROM album_adds WHERE account=?1 AND node=?2 AND album=?3")?;
                for (phone, album) in &done {
                    delete.execute(params![account.as_str(), phone, album.as_str()])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}
