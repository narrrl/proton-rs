//! The phone's own photo library beside Proton Photos.
//!
//! The host (Android) reads the system media index and hands over metadata in
//! batches; files are never copied. Each phone item is matched against the
//! signed-in account's Proton Photos: once a copy is known, the timeline shows
//! the Proton photo (with the phone file as a faster preview) instead of both.
use rusqlite::{OptionalExtension, params};

use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};

const PHONE: &str = "phone";
const PHONE_VOLUME: &str = "media";
/// Cursor marker of the merged Photos timeline.
const TIMELINE: &str = "timeline";

pub fn phone_source() -> Result<SourceSpec> {
    Ok(SourceSpec {
        id: SourceId::new(PHONE)?,
        account: AccountId::new(PHONE)?,
        volume: VolumeId::new(PHONE_VOLUME)?,
        root: NodeId::new(PHONE_VOLUME)?,
        label: "This phone".into(),
    })
}

pub fn phone_key(id: &str) -> Result<AssetKey> {
    Ok(AssetKey {
        account: AccountId::new(PHONE)?,
        volume: VolumeId::new(PHONE_VOLUME)?,
        node: NodeId::new(id)?,
    })
}

pub fn is_phone(key: &AssetKey) -> bool {
    key.account.as_str() == PHONE && key.volume.as_str() == PHONE_VOLUME
}

/// One entry of the system media index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneMedia {
    /// The system's identifier, stable while the file exists.
    pub id: String,
    pub name: String,
    pub mime: String,
    /// The folder (bucket) the file is in, and its display name.
    pub folder: String,
    pub folder_name: String,
    /// When the photo was taken, otherwise when the file last changed (seconds).
    pub taken: i64,
    pub modified: i64,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub favorite: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackupState {
    Unchecked,
    Missing,
    /// A Proton photo has the same name; contents are compared next.
    Maybe,
    BackedUp {
        volume: VolumeId,
        node: NodeId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneFolder {
    pub id: String,
    pub name: String,
    pub count: u64,
    /// Items not known to be in Proton, for the signed-in account.
    pub not_backed_up: u64,
    pub backup: bool,
}

/// The phone side of a timeline item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhoneLink {
    /// The phone's identifier when the file is on this phone.
    pub phone: Option<String>,
    /// Whether Proton holds a copy. Proton photos are always backed up.
    pub backed_up: bool,
}

/// A phone item whose Proton copy is known, see [`Catalog::freeable`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreeableMedia {
    pub id: String,
    pub is_video: bool,
    pub name: String,
    pub sha1: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhoneEntry {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub folder_name: String,
    pub taken: i64,
    pub size_bytes: u64,
    pub favorite: bool,
    pub sha1: Option<String>,
    pub state: BackupState,
}

/// Whether timeline row `m` (joined to asset `a`) is shown. ?1 is the Photos
/// source, ?2 the account; both may be NULL. A phone item is hidden while its
/// Proton copy is in the timeline, and while a same-named Proton photo is
/// there and the contents have not been compared, so a backed-up library
/// never shows every photo twice during the first check.
///
/// Media in a hidden album is left out too (see [`NOT_HIDDEN`]), and so is a
/// phone item whose Proton copy is in one.
pub(crate) const VISIBLE: &str = "((m.source=?1 OR (m.source='phone' AND NOT EXISTS(
    SELECT 1 FROM phone_backup b WHERE b.account=?2 AND b.node=m.node AND (
        (b.state='backed_up' AND EXISTS(SELECT 1 FROM memberships r
            WHERE r.source=?1 AND r.account=?2 AND r.volume=b.remote_volume AND r.node=b.remote_node))
        OR (b.state='maybe' AND EXISTS(SELECT 1 FROM memberships r
            WHERE r.source=?1 AND r.sort_name=m.sort_name))))
    AND NOT (?2 IS NOT NULL
        AND NOT EXISTS(SELECT 1 FROM phone_backup b WHERE b.account=?2 AND b.node=m.node)
        AND EXISTS(SELECT 1 FROM memberships r
            WHERE r.source=?1 AND r.sort_name=m.sort_name))))
    AND NOT EXISTS(SELECT 1 FROM memberships h JOIN album_settings s ON s.source=h.source
        WHERE s.hidden=1 AND h.source<>m.source AND (h.account,h.volume,h.node)=(m.account,m.volume,m.node))
    AND NOT (m.source='phone' AND EXISTS(SELECT 1 FROM phone_backup b
        JOIN memberships h ON (h.account,h.volume,h.node)=(b.account,b.remote_volume,b.remote_node)
        JOIN album_settings s ON s.source=h.source
        WHERE b.account=?2 AND b.node=m.node AND b.state='backed_up' AND s.hidden=1 AND h.source IS NOT ?1)))";

/// Whether the media `alias` names is in no hidden album. Hidden albums are a
/// choice made on this phone: Proton cannot hide photos, so the albums stay
/// in Proton and only this app leaves their media out of shared views such as
/// the timeline and the system photo picker. `album_settings.hidden` of the
/// timeline's own source is never set; the album view shows its own media.
pub(crate) fn not_hidden(alias: &str) -> String {
    format!(
        "NOT EXISTS(SELECT 1 FROM memberships h JOIN album_settings s ON s.source=h.source
            WHERE s.hidden=1 AND (h.account,h.volume,h.node)=({alias}.account,{alias}.volume,{alias}.node))"
    )
}

fn state(row: Option<(String, Option<String>, Option<String>)>) -> Result<BackupState> {
    Ok(match row {
        None => BackupState::Unchecked,
        Some((state, volume, node)) => match (state.as_str(), volume, node) {
            ("missing", ..) => BackupState::Missing,
            ("maybe", ..) => BackupState::Maybe,
            ("backed_up", Some(volume), Some(node)) => BackupState::BackedUp {
                volume: VolumeId::new(volume)?,
                node: NodeId::new(node)?,
            },
            _ => return Err(Error::Invalid("unknown backup state".into())),
        },
    })
}

fn text(value: Option<&AccountId>) -> rusqlite::types::Value {
    value.map_or(rusqlite::types::Value::Null, |v| {
        v.as_str().to_owned().into()
    })
}

/// Remove phone items and everything recorded about them.
fn forget_phone_media(tx: &rusqlite::Transaction<'_>, nodes: &[String]) -> Result<()> {
    for node in nodes {
        tx.execute("DELETE FROM phone_backup WHERE node=?1", [node])?;
        tx.execute("DELETE FROM phone_media WHERE node=?1", [node])?;
        for table in ["memberships", "media_shapes", "assets"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE account=?1 AND volume=?2 AND node=?3"),
                params![PHONE, PHONE_VOLUME, node],
            )?;
        }
    }
    Ok(())
}

impl Catalog {
    /// Record one batch of a scan. Unchanged files keep their check results;
    /// a changed file is compared again.
    pub async fn phone_upsert(&self, scan: i64, items: Vec<PhoneMedia>) -> Result<()> {
        if items.len() > 2_000 {
            return Err(Error::Invalid("phone batch is too large".into()));
        }
        let mut accepted = Vec::with_capacity(items.len());
        for item in items {
            let Some(kind) = MediaKind::classify(&item.mime, &item.name) else {
                continue;
            };
            if item.name.is_empty()
                || item.name.len() > 1024
                || item.mime.len() > 255
                || item.folder.len() > 1024
                || item.folder_name.len() > 1024
                || item.size_bytes > i64::MAX as u64
                || item.width > 1_000_000
                || item.height > 1_000_000
            {
                return Err(Error::Invalid("phone media metadata exceeds bounds".into()));
            }
            let asset = Asset {
                key: phone_key(&item.id)?,
                name: item.name.clone(),
                kind,
                mime: item.mime.clone(),
                revision: None,
                size_bytes: Some(item.size_bytes),
                sort_time: item.taken,
                favorite: item.favorite,
            };
            accepted.push((item, serde_json::to_string(&asset)?));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut known = tx.prepare_cached("SELECT modified,size_bytes FROM phone_media WHERE node=?1")?;
                let mut keep = tx.prepare_cached("UPDATE phone_media SET folder=?2,folder_name=?3,favorite=?4,scan=max(scan,?5) WHERE node=?1")?;
                let mut replace = tx.prepare_cached(
                    "INSERT INTO phone_media(node,folder,folder_name,modified,size_bytes,favorite,sha1,scan) VALUES (?1,?2,?3,?4,?5,?6,NULL,?7)
                     ON CONFLICT(node) DO UPDATE SET folder=excluded.folder,folder_name=excluded.folder_name,modified=excluded.modified,
                     size_bytes=excluded.size_bytes,favorite=excluded.favorite,sha1=NULL,scan=max(scan,excluded.scan)")?;
                let mut forget = tx.prepare_cached("DELETE FROM phone_backup WHERE node=?1")?;
                let mut assets = tx.prepare_cached(
                    "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(account,volume,node) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload")?;
                let mut members = tx.prepare_cached("INSERT OR IGNORE INTO memberships(source,account,volume,node) VALUES ('phone',?1,?2,?3)")?;
                let mut shape = tx.prepare_cached(
                    "INSERT INTO media_shapes(account,volume,node,width,height) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(account,volume,node) DO UPDATE SET width=excluded.width,height=excluded.height")?;
                for (item, payload) in &accepted {
                    let size = item.size_bytes as i64;
                    let previous: Option<(i64, i64)> = known.query_row([&item.id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    if previous == Some((item.modified, size)) {
                        keep.execute(params![item.id, item.folder, item.folder_name, item.favorite, scan])?;
                    } else {
                        replace.execute(params![item.id, item.folder, item.folder_name, item.modified, size, item.favorite, scan])?;
                        forget.execute([&item.id])?;
                    }
                    assets.execute(params![PHONE, PHONE_VOLUME, item.id, item.taken, payload])?;
                    members.execute(params![PHONE, PHONE_VOLUME, item.id])?;
                    if item.width > 0 && item.height > 0 {
                        shape.execute(params![PHONE, PHONE_VOLUME, item.id, item.width, item.height])?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// End a complete scan: forget files it did not see. Returns how many.
    /// Only older scans count: a later scan that overlapped this one has
    /// seen its files already, and their comparison results must survive.
    pub async fn phone_finish(&self, scan: i64) -> Result<u64> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let gone: Vec<String> = {
                let mut query = tx.prepare("SELECT node FROM phone_media WHERE scan<?1")?;
                query
                    .query_map([scan], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            forget_phone_media(&tx, &gone)?;
            // Backup folders stay chosen while empty, e.g. after freeing space.
            tx.commit()?;
            Ok(gone.len() as u64)
        })
        .await
    }

    /// Compare the catalog with every identifier the phone shows now, without
    /// reading metadata again: forget items that are gone, and return the
    /// identifiers the catalog does not know yet with the number forgotten.
    pub async fn phone_retain(&self, ids: Vec<String>) -> Result<(u64, Vec<String>)> {
        if ids.len() > 2_000_000 || ids.iter().any(|id| id.is_empty() || id.len() > 64) {
            return Err(Error::Invalid("phone identifiers exceed bounds".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS phone_seen(node TEXT PRIMARY KEY NOT NULL);
                 DELETE FROM temp.phone_seen;",
            )?;
            {
                let mut seen = tx.prepare_cached("INSERT OR IGNORE INTO temp.phone_seen(node) VALUES (?1)")?;
                for id in &ids {
                    seen.execute([id])?;
                }
            }
            let gone: Vec<String> = tx
                .prepare("SELECT node FROM phone_media WHERE node NOT IN (SELECT node FROM temp.phone_seen)")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let unknown: Vec<String> = tx
                .prepare("SELECT node FROM temp.phone_seen WHERE node NOT IN (SELECT node FROM phone_media)")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            forget_phone_media(&tx, &gone)?;
            tx.execute("DELETE FROM temp.phone_seen", [])?;
            tx.commit()?;
            Ok((gone.len() as u64, unknown))
        })
        .await
    }

    pub async fn phone_folders(&self, account: Option<AccountId>) -> Result<Vec<PhoneFolder>> {
        self.run(move |conn| {
            let mut query = conn.prepare(
                "SELECT p.folder,max(p.folder_name),count(*),
                    sum(NOT EXISTS(SELECT 1 FROM phone_backup b WHERE b.account=?1 AND b.node=p.node AND b.state='backed_up')),
                    EXISTS(SELECT 1 FROM backup_folders f WHERE f.folder=p.folder)
                 FROM phone_media p GROUP BY p.folder ORDER BY count(*) DESC, max(p.folder_name)",
            )?;
            let rows = query.query_map([text(account.as_ref())], |r| {
                Ok(PhoneFolder {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    count: r.get::<_, i64>(2)? as u64,
                    not_backed_up: r.get::<_, i64>(3)? as u64,
                    backup: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    pub async fn set_backup_folder(&self, folder: String, enabled: bool) -> Result<()> {
        self.run(move |conn| {
            if enabled {
                conn.execute(
                    "INSERT OR IGNORE INTO backup_folders(folder) VALUES (?1)",
                    [folder],
                )?;
            } else {
                conn.execute("DELETE FROM backup_folders WHERE folder=?1", [folder])?;
            }
            Ok(())
        })
        .await
    }

    /// How many phone items still need comparing with `account`: never
    /// compared, or same-named and waiting for their contents.
    pub async fn phone_remaining(&self, account: AccountId) -> Result<u64> {
        self.run(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM phone_media p LEFT JOIN phone_backup b ON b.account=?1 AND b.node=p.node
                 WHERE b.node IS NULL OR b.state='maybe'",
                [account.as_str()],
                |r| r.get(0),
            )?;
            Ok(count as u64)
        })
        .await
    }

    /// Items found missing by name alone whose name has since appeared in
    /// `source` (another app may have uploaded them): compare them again.
    /// Items whose contents were compared keep their result. Returns how many.
    pub async fn phone_recheck_new_names(
        &self,
        account: AccountId,
        source: SourceId,
    ) -> Result<u64> {
        self.run(move |conn| {
            let changed = conn.execute(
                "DELETE FROM phone_backup WHERE account=?1 AND state='missing' AND node IN (
                    SELECT p.node FROM phone_media p
                    JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                    WHERE p.sha1 IS NULL AND EXISTS(SELECT 1 FROM memberships r WHERE r.source=?2 AND r.sort_name=a.sort_name))",
                params![account.as_str(), source.as_str()],
            )?;
            Ok(changed as u64)
        })
        .await
    }

    /// Phone items not yet compared with `account`, newest first: `(id, name)`.
    pub async fn phone_unchecked(
        &self,
        account: AccountId,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT p.node,json_extract(a.payload,'$.name') FROM phone_media p
                 JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                 WHERE NOT EXISTS(SELECT 1 FROM phone_backup b WHERE b.account=?1 AND b.node=p.node)
                 ORDER BY a.sort_time DESC LIMIT ?2",
            )?;
            let rows = query.query_map(params![account.as_str(), limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Same-named items whose contents still need hashing (`sha1` NULL):
    /// `(id, is_video)`.
    pub async fn phone_unhashed(
        &self,
        account: AccountId,
        limit: usize,
        skip: Vec<String>,
    ) -> Result<Vec<(String, bool)>> {
        let skip = serde_json::to_string(&skip)?;
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT p.node,json_extract(a.payload,'$.kind')='video' FROM phone_media p
                 JOIN phone_backup b ON b.node=p.node AND b.account=?1
                 JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                 WHERE b.state='maybe' AND p.sha1 IS NULL
                 AND p.node NOT IN (SELECT value FROM json_each(?3))
                 ORDER BY a.sort_time DESC LIMIT ?2",
            )?;
            let rows = query.query_map(params![account.as_str(), limit as i64, skip], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Same-named, hashed items ready for the content comparison:
    /// `(id, name, sha1)`.
    pub async fn phone_hashed(
        &self,
        account: AccountId,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT p.node,json_extract(a.payload,'$.name'),p.sha1 FROM phone_media p
                 JOIN phone_backup b ON b.node=p.node AND b.account=?1
                 JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                 WHERE b.state='maybe' AND p.sha1 IS NOT NULL LIMIT ?2",
            )?;
            let rows = query.query_map(params![account.as_str(), limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Store contents hashes for phone items that still exist.
    pub async fn phone_set_hashes(&self, hashes: Vec<(String, String)>) -> Result<()> {
        for (_, sha1) in &hashes {
            if sha1.len() != 40
                || !sha1
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(Error::Invalid("invalid SHA-1 digest".into()));
            }
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut update =
                    tx.prepare_cached("UPDATE phone_media SET sha1=?2 WHERE node=?1")?;
                for (node, sha1) in &hashes {
                    update.execute(params![node, sha1])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Record comparison results for `account`. `Unchecked` forgets a result.
    pub async fn phone_mark(
        &self,
        account: AccountId,
        results: Vec<(String, BackupState)>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut set = tx.prepare_cached(
                    "INSERT INTO phone_backup(account,node,state,remote_volume,remote_node)
                     SELECT ?1,?2,?3,?4,?5 WHERE EXISTS(SELECT 1 FROM phone_media WHERE node=?2)
                     ON CONFLICT(account,node) DO UPDATE SET state=excluded.state,
                     remote_volume=excluded.remote_volume,remote_node=excluded.remote_node",
                )?;
                let mut clear =
                    tx.prepare_cached("DELETE FROM phone_backup WHERE account=?1 AND node=?2")?;
                for (node, state) in &results {
                    match state {
                        BackupState::Unchecked => {
                            clear.execute(params![account.as_str(), node])?;
                        }
                        BackupState::Missing => {
                            set.execute(params![
                                account.as_str(),
                                node,
                                "missing",
                                None::<String>,
                                None::<String>
                            ])?;
                        }
                        BackupState::Maybe => {
                            set.execute(params![
                                account.as_str(),
                                node,
                                "maybe",
                                None::<String>,
                                None::<String>
                            ])?;
                        }
                        BackupState::BackedUp {
                            volume,
                            node: remote,
                        } => {
                            set.execute(params![
                                account.as_str(),
                                node,
                                "backed_up",
                                volume.as_str(),
                                remote.as_str()
                            ])?;
                        }
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn phone_entry(
        &self,
        id: String,
        account: Option<AccountId>,
    ) -> Result<Option<PhoneEntry>> {
        self.run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT a.payload,p.folder_name,p.favorite,p.sha1,b.state,b.remote_volume,b.remote_node
                     FROM phone_media p JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                     LEFT JOIN phone_backup b ON b.node=p.node AND b.account=?2 WHERE p.node=?1",
                    params![id, text(account.as_ref())],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, bool>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((payload, folder_name, favorite, sha1, kind, volume, node)) = row else {
                return Ok(None);
            };
            let asset: Asset = serde_json::from_str(&payload)?;
            Ok(Some(PhoneEntry {
                id,
                name: asset.name,
                mime: asset.mime,
                folder_name,
                taken: asset.sort_time,
                size_bytes: asset.size_bytes.unwrap_or(0),
                favorite,
                sha1,
                state: state(kind.map(|k| (k, volume, node)))?,
            }))
        })
        .await
    }

    /// Items in backup folders that `account` does not hold yet, newest
    /// first: `(id, is_video)`.
    pub async fn backup_queue(
        &self,
        account: AccountId,
        limit: usize,
    ) -> Result<Vec<(String, bool)>> {
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT p.node,json_extract(a.payload,'$.kind')='video' FROM phone_media p JOIN backup_folders f ON f.folder=p.folder
                 JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                 WHERE NOT EXISTS(SELECT 1 FROM phone_backup b WHERE b.account=?1 AND b.node=p.node AND b.state='backed_up')
                 ORDER BY a.sort_time DESC LIMIT ?2",
            )?;
            let rows = query.query_map(params![account.as_str(), limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// Backed-up phone items that may leave the phone: their Proton copy is
    /// in the synced Photos `source`, their contents hash is known, and they
    /// were taken before `before` (seconds). Oldest first:
    /// `(id, is_video, name, sha1, size_bytes)`.
    pub async fn freeable(
        &self,
        account: AccountId,
        source: SourceId,
        before: i64,
    ) -> Result<Vec<FreeableMedia>> {
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT p.node,json_extract(a.payload,'$.kind')='video',json_extract(a.payload,'$.name'),p.sha1,p.size_bytes
                 FROM phone_media p
                 JOIN phone_backup b ON b.account=?1 AND b.node=p.node AND b.state='backed_up'
                 JOIN assets a ON a.account='phone' AND a.volume='media' AND a.node=p.node
                 WHERE p.sha1 IS NOT NULL AND a.sort_time<?3
                   AND EXISTS(SELECT 1 FROM memberships r WHERE r.source=?2 AND r.account=?1
                       AND r.volume=b.remote_volume AND r.node=b.remote_node)
                 ORDER BY a.sort_time",
            )?;
            let rows = query.query_map(params![account.as_str(), source.as_str(), before], |r| {
                Ok(FreeableMedia {
                    id: r.get(0)?,
                    is_video: r.get(1)?,
                    name: r.get(2)?,
                    sha1: r.get(3)?,
                    size_bytes: r.get::<_, i64>(4)? as u64,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// The phone side of each asset, in input order.
    pub async fn phone_links(
        &self,
        account: Option<AccountId>,
        keys: Vec<AssetKey>,
    ) -> Result<Vec<PhoneLink>> {
        if keys.len() > PageSize::MAX {
            return Err(Error::Invalid("too many items requested".into()));
        }
        self.run(move |conn| {
            let mut own = conn.prepare_cached("SELECT state='backed_up' FROM phone_backup WHERE account=?1 AND node=?2")?;
            let mut copy = conn.prepare_cached(
                "SELECT b.node FROM phone_backup b WHERE b.account=?1 AND b.remote_volume=?2 AND b.remote_node=?3 LIMIT 1",
            )?;
            keys.into_iter()
                .map(|key| {
                    if is_phone(&key) {
                        let backed_up = match &account {
                            Some(account) => own.query_row(params![account.as_str(), key.node.as_str()], |r| r.get(0)).optional()?.unwrap_or(false),
                            None => false,
                        };
                        Ok(PhoneLink { phone: Some(key.node.as_str().to_owned()), backed_up })
                    } else {
                        let phone = copy
                            .query_row(params![key.account.as_str(), key.volume.as_str(), key.node.as_str()], |r| r.get(0))
                            .optional()?;
                        Ok(PhoneLink { phone, backed_up: true })
                    }
                })
                .collect()
        })
        .await
    }

    /// Give a Proton copy the known shape of its phone original.
    pub async fn copy_shape(&self, from: AssetKey, to: AssetKey) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO media_shapes(account,volume,node,width,height)
                 SELECT ?4,?5,?6,width,height FROM media_shapes WHERE account=?1 AND volume=?2 AND node=?3
                 AND EXISTS(SELECT 1 FROM assets WHERE account=?4 AND volume=?5 AND node=?6)",
                params![from.account.as_str(), from.volume.as_str(), from.node.as_str(), to.account.as_str(), to.volume.as_str(), to.node.as_str()],
            )?;
            Ok(())
        })
        .await
    }

    /// Add or update one Proton photo (e.g. just uploaded) in a source.
    pub async fn add_to_source(&self, source: SourceId, asset: Asset) -> Result<()> {
        let payload = serde_json::to_string(&asset)?;
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(account,volume,node) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload",
                params![asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str(), asset.sort_time, payload],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)",
                params![source.as_str(), asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str()],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// The Photos timeline: Proton photos of `photos` and phone items without a
    /// Proton copy in it, newest first.
    pub async fn timeline_window(
        &self,
        photos: Option<SourceId>,
        account: Option<AccountId>,
        after: Option<TimelineCursor>,
        backwards: bool,
        size: PageSize,
    ) -> Result<TimelineWindow> {
        let marker = SourceId::new(TIMELINE)?;
        if after
            .as_ref()
            .is_some_and(|c| c.source.as_ref() != Some(&marker) || c.order != SortOrder::Newest)
        {
            return Err(Error::Invalid("cursor belongs to a different view".into()));
        }
        if backwards && after.is_none() {
            return Err(Error::Invalid("backwards paging needs a cursor".into()));
        }
        self.run(move |conn| {
            let mut sql = format!("SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node) WHERE {VISIBLE}");
            let mut values: Vec<rusqlite::types::Value> = vec![
                photos.as_ref().map_or(rusqlite::types::Value::Null, |s| s.as_str().to_owned().into()),
                text(account.as_ref()),
            ];
            let descending = !backwards;
            if let Some(after) = &after {
                let comparison = if descending { "<" } else { ">" };
                sql.push_str(&format!(" AND (m.sort_time,m.account,m.volume,m.node){comparison}(?3,?4,?5,?6)"));
                values.extend([
                    after.time.into(),
                    after.key.account.as_str().to_owned().into(),
                    after.key.volume.as_str().to_owned().into(),
                    after.key.node.as_str().to_owned().into(),
                ]);
            }
            let direction = if descending { "DESC" } else { "ASC" };
            sql.push_str(&format!(
                " ORDER BY m.sort_time {direction},m.account {direction},m.volume {direction},m.node {direction} LIMIT ?{}",
                values.len() + 1
            ));
            values.push(((size.get() + 1) as i64).into());
            let mut query = conn.prepare_cached(&sql)?;
            let rows = query.query_map(rusqlite::params_from_iter(values), |r| r.get::<_, String>(0))?;
            let mut items: Vec<Asset> = rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect::<Result<_>>()?;
            let more = items.len() > size.get();
            items.truncate(size.get());
            if backwards {
                items.reverse();
            }
            let cursor = |a: &Asset| TimelineCursor {
                source: Some(marker.clone()),
                time: a.sort_time,
                key: a.key.clone(),
                order: SortOrder::Newest,
                name: String::new(),
            };
            let next = if (backwards && after.is_some()) || (!backwards && more) { items.last().map(cursor) } else { None };
            let previous = if (backwards && more) || (!backwards && after.is_some()) { items.first().map(cursor) } else { None };
            Ok(TimelineWindow { items, next, previous })
        })
        .await
    }

    pub async fn timeline_count(
        &self,
        photos: Option<SourceId>,
        account: Option<AccountId>,
    ) -> Result<u64> {
        self.run(move |conn| {
            let count: i64 = conn.query_row(
                &format!("SELECT count(*) FROM memberships m JOIN assets a USING(account,volume,node) WHERE {VISIBLE}"),
                params![photos.as_ref().map(|s| s.as_str().to_owned()), account.as_ref().map(|a| a.as_str().to_owned())],
                |r| r.get(0),
            )?;
            Ok(count as u64)
        })
        .await
    }
}
