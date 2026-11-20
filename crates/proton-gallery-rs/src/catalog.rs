//! A bounded asynchronous facade over a single dedicated SQLite worker.
//!
//! Callers never execute SQL or wait for a filesystem on their UI thread.
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tokio::sync::{mpsc, oneshot};

use crate::bytes::RevisionDescriptor;
use crate::model::*;
use crate::source::SourcePage;
use crate::{Error, Result};

const SCHEMA_VERSION: u32 = 14;
const APPLICATION_ID: u32 = 0x50474c59;
type Job = Box<dyn FnOnce(&mut Connection) + Send>;

#[derive(Clone)]
pub struct Catalog {
    jobs: mpsc::Sender<Job>,
}

impl Catalog {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::start(Some(path.as_ref().to_owned())).await
    }

    pub async fn in_memory() -> Result<Self> {
        Self::start(None).await
    }

    async fn start(path: Option<PathBuf>) -> Result<Self> {
        let (jobs, mut receiver) = mpsc::channel::<Job>(64);
        let (ready, waiting) = oneshot::channel();
        std::thread::Builder::new()
            .name("gallery-catalog".into())
            .spawn(move || {
                let opened = open_connection(path.as_deref());
                match opened {
                    Ok(mut connection) => {
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        while let Some(job) = receiver.blocking_recv() {
                            job(&mut connection);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        waiting.await.map_err(|_| Error::CatalogClosed)??;
        Ok(Self { jobs })
    }

    pub(crate) async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (answer, waiting) = oneshot::channel();
        self.jobs
            .send(Box::new(move |connection| {
                let _ = answer.send(operation(connection));
            }))
            .await
            .map_err(|_| Error::CatalogClosed)?;
        waiting.await.map_err(|_| Error::CatalogClosed)?
    }

    pub async fn register_source(&self, source: SourceSpec) -> Result<()> {
        self.run(move |conn| {
            let previous: Option<String> = conn
                .query_row(
                    "SELECT payload FROM sources WHERE id=?1",
                    [source.id.as_str()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(previous) = previous {
                let previous: SourceSpec = serde_json::from_str(&previous)?;
                if previous.account != source.account
                    || previous.volume != source.volume
                    || previous.root != source.root
                {
                    return Err(Error::Invalid(
                        "source identity cannot be rebound to another scope".into(),
                    ));
                }
            }
            conn.execute(
                "INSERT INTO sources(id,payload) VALUES (?1,?2)
                 ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
                params![source.id.as_str(), serde_json::to_string(&source)?],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn sources(&self) -> Result<Vec<SourceSpec>> {
        self.run(|conn| {
            let mut query = conn.prepare("SELECT payload FROM sources ORDER BY id")?;
            let rows = query.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
        })
        .await
    }

    /// Resume a checkpoint, or create a new generation for an explicit refresh.
    pub async fn begin_scan(&self, source: SourceId, restart: bool) -> Result<ScanCheckpoint> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let old = checkpoint(&tx, &source)?;
            let next = match old {
                Some(old) if !restart => old,
                old => {
                    let generation = old.map_or(Ok(1), |old| old.generation.checked_add(1).ok_or_else(|| Error::Invalid("scan generation exhausted".into())))?;
                    let next = ScanCheckpoint { generation, cursor: None, complete: false, indexed: 0 };
                    tx.execute(
                        "INSERT INTO scans(source,generation,cursor,complete,indexed) VALUES (?1,?2,NULL,0,0)
                         ON CONFLICT(source) DO UPDATE SET generation=excluded.generation,cursor=NULL,complete=0,indexed=0",
                        params![source.as_str(), sql_integer(generation)?],
                    )?;
                    next
                }
            };
            tx.commit()?;
            Ok(next)
        }).await
    }

    /// Metadata, membership, and continuation become visible in one transaction.
    pub async fn apply_page(
        &self,
        source: SourceSpec,
        expected: ScanCheckpoint,
        page: SourcePage,
    ) -> Result<ScanCheckpoint> {
        if page.assets.len() > PageSize::MAX || page.next.as_ref().is_some_and(|s| s.len() > 8192) {
            return Err(Error::Invalid("remote page exceeds bounds".into()));
        }
        if page.next.is_some() && page.next == expected.cursor {
            return Err(Error::Invalid("source did not advance its cursor".into()));
        }
        for asset in &page.assets {
            if asset.key.account != source.account || asset.key.volume != source.volume {
                return Err(Error::Invalid(
                    "asset does not belong to this source account and volume".into(),
                ));
            }
            if asset.name.len() > 16_384
                || asset.mime.len() > 1024
                || asset.size_bytes.is_some_and(|size| size > i64::MAX as u64)
            {
                return Err(Error::Invalid("asset metadata exceeds bounds".into()));
            }
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            if checkpoint(&tx, &source.id)?.as_ref() != Some(&expected) || expected.complete {
                return Err(Error::StaleScan);
            }
            let registered: String = tx.query_row("SELECT payload FROM sources WHERE id=?1", [source.id.as_str()], |r| r.get(0))?;
            let registered: SourceSpec = serde_json::from_str(&registered)?;
            if registered.account != source.account || registered.volume != source.volume || registered.root != source.root {
                return Err(Error::StaleScan);
            }
            {
                let mut assets = tx.prepare_cached(
                    "INSERT INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(account,volume,node) DO UPDATE SET sort_time=excluded.sort_time,payload=excluded.payload",
                )?;
                let mut members = tx.prepare_cached(
                    "INSERT OR IGNORE INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)",
                )?;
                for asset in &page.assets {
                    assets.execute(params![asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str(), asset.sort_time, serde_json::to_string(asset)?])?;
                    members.execute(params![source.id.as_str(), asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str()])?;
                }
            }
            let next = ScanCheckpoint {
                generation: expected.generation,
                complete: page.next.is_none(),
                cursor: page.next,
                indexed: expected.indexed.checked_add(page.assets.len() as u64).ok_or_else(|| Error::Invalid("scan count exhausted".into()))?,
            };
            tx.execute(
                "UPDATE scans SET cursor=?2,complete=?3,indexed=?4 WHERE source=?1",
                params![source.id.as_str(), next.cursor, next.complete, sql_integer(next.indexed)?],
            )?;
            // Absence from a scan is deliberately not a deletion signal. Event
            // reconciliation is a later feature; failed scans must never erase data.
            tx.commit()?;
            Ok(next)
        }).await
    }

    pub async fn page(
        &self,
        source: Option<SourceId>,
        after: Option<TimelineCursor>,
        size: PageSize,
    ) -> Result<AssetPage> {
        let window = self.window(source, after, false, size).await?;
        Ok(AssetPage {
            items: window.items,
            next: window.next,
        })
    }

    pub async fn window(
        &self,
        source: Option<SourceId>,
        after: Option<TimelineCursor>,
        backwards: bool,
        size: PageSize,
    ) -> Result<TimelineWindow> {
        self.window_ordered(source, after, backwards, SortOrder::Newest, size)
            .await
    }

    /// Pages in `order`. A cursor only continues the order it was issued for.
    pub async fn window_ordered(
        &self,
        source: Option<SourceId>,
        after: Option<TimelineCursor>,
        backwards: bool,
        order: SortOrder,
        size: PageSize,
    ) -> Result<TimelineWindow> {
        if after.as_ref().is_some_and(|cursor| cursor.order != order) {
            return Err(Error::Invalid(
                "cursor belongs to a different sort order".into(),
            ));
        }
        if backwards && after.is_none() {
            return Err(Error::Invalid("backwards paging needs a cursor".into()));
        }
        if after.as_ref().is_some_and(|cursor| cursor.source != source) {
            return Err(Error::Invalid(
                "cursor belongs to a different source filter".into(),
            ));
        }
        self.run(move |conn| {
            // ORDER BY and keyset comparison use the same complete unique key.
            // Keep the seek predicate out of an OR: SQLite can then seek into
            // the index instead of scanning all preceding pages.
            let (mut sql, ordered) = if source.is_some() {
                (String::from("SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node) WHERE m.source=?1"), "m")
            } else {
                (String::from("SELECT a.payload FROM assets a WHERE ?1 IS NULL"), "a")
            };
            let mut values = vec![source.as_ref().map_or(rusqlite::types::Value::Null, |s| {
                s.as_str().to_owned().into()
            })];
            // Both name columns are NOCASE, so comparison and index agree.
            let first = if order.by_name() { "sort_name" } else { "sort_time" };
            let descending = backwards == order.ascending();
            if let Some(after) = &after {
                let comparison = if descending { "<" } else { ">" };
                sql.push_str(&format!(" AND ({ordered}.{first},{ordered}.account,{ordered}.volume,{ordered}.node){comparison}(?2,?3,?4,?5)"));
                values.extend([
                    if order.by_name() { after.name.clone().into() } else { after.time.into() },
                    after.key.account.as_str().to_owned().into(),
                    after.key.volume.as_str().to_owned().into(),
                    after.key.node.as_str().to_owned().into(),
                ]);
            }
            let direction = if descending { "DESC" } else { "ASC" };
            sql.push_str(&format!(
                " ORDER BY {ordered}.{first} {direction},{ordered}.account {direction},{ordered}.volume {direction},{ordered}.node {direction} LIMIT ?{}", values.len() + 1));
            values.push(((size.get() + 1) as i64).into());
            let mut query = conn.prepare_cached(&sql)?;
            let rows = query.query_map(rusqlite::params_from_iter(values), |r| {
                r.get::<_, String>(0)
            })?;
            let mut items: Vec<Asset> = rows
                .map(|r| Ok(serde_json::from_str(&r?)?))
                .collect::<Result<_>>()?;
            let more = items.len() > size.get();
            items.truncate(size.get());
            if backwards { items.reverse(); }
            let cursor = |a: &Asset| TimelineCursor {
                    source: source.clone(),
                    time: a.sort_time,
                    key: a.key.clone(),
                    order,
                    name: if order.by_name() { a.name.clone() } else { String::new() },
            };
            let next = if (backwards && after.is_some()) || (!backwards && more) {
                items.last().map(cursor)
            } else { None };
            let previous = if (backwards && more) || (!backwards && after.is_some()) {
                items.first().map(cursor)
            } else { None };
            Ok(TimelineWindow { items, next, previous })
        })
        .await
    }

    pub async fn stats(&self) -> Result<CatalogStats> {
        self.run(|conn| {
            Ok(CatalogStats {
                sources: conn.query_row("SELECT count(*) FROM sources", [], |r| {
                    unsigned_column(r, 0)
                })?,
                assets: conn
                    .query_row("SELECT count(*) FROM assets", [], |r| unsigned_column(r, 0))?,
                retained_revisions: conn.query_row(
                    "SELECT count(*) FROM offline WHERE complete=1",
                    [],
                    |r| unsigned_column(r, 0),
                )?,
            })
        })
        .await
    }

    /// The durable file has already been published before this atomic metadata commit.
    pub(crate) async fn commit_local(
        &self,
        source: SourceSpec,
        asset: Asset,
        hash: String,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("INSERT OR IGNORE INTO sources(id,payload) VALUES (?1,?2)",
                params![source.id.as_str(), serde_json::to_string(&source)?])?;
            tx.execute("INSERT OR IGNORE INTO source_kinds(source,kind) VALUES (?1,'device')",[source.id.as_str()])?;
            // Reimporting identical bytes preserves the original title and timeline position.
            tx.execute("INSERT OR IGNORE INTO assets(account,volume,node,sort_time,payload) VALUES (?1,?2,?3,?4,?5)",
                params![asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str(), asset.sort_time, serde_json::to_string(&asset)?])?;
            tx.execute("INSERT OR IGNORE INTO memberships(source,account,volume,node) VALUES (?1,?2,?3,?4)",
                params![source.id.as_str(), asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str()])?;
            tx.execute("INSERT OR IGNORE INTO local_media(account,volume,node,content_hash,size_bytes) VALUES (?1,?2,?3,?4,?5)",
                params![asset.key.account.as_str(), asset.key.volume.as_str(), asset.key.node.as_str(), hash, sql_integer(asset.size_bytes.unwrap_or(0))?])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn local_hash(&self, key: AssetKey) -> Result<Option<String>> {
        Ok(self.local_hashes(vec![key]).await?.pop().flatten())
    }

    pub async fn local_hashes(&self, keys: Vec<AssetKey>) -> Result<Vec<Option<String>>> {
        if keys.len() > PageSize::MAX {
            return Err(Error::Invalid("too many local file requests".into()));
        }
        self.run(move |conn| {
            let mut query = conn.prepare_cached(
                "SELECT content_hash FROM local_media WHERE (account,volume,node)=(?1,?2,?3)",
            )?;
            keys.into_iter()
                .map(|key| {
                    Ok(query
                        .query_row(
                            params![key.account.as_str(), key.volume.as_str(), key.node.as_str()],
                            |r| r.get(0),
                        )
                        .optional()?)
                })
                .collect()
        })
        .await
    }

    pub async fn local_totals(&self) -> Result<(u64, u64)> {
        self.run(|conn| {
            Ok(conn.query_row(
                "SELECT count(*),coalesce(sum(size_bytes),0) FROM local_media",
                [],
                |row| Ok((unsigned_column(row, 0)?, unsigned_column(row, 1)?)),
            )?)
        })
        .await
    }

    pub async fn remember_revision(&self, descriptor: RevisionDescriptor) -> Result<()> {
        self.run(move |conn| {
            let key = revision_key(&descriptor)?;
            let old: Option<String> = conn
                .query_row("SELECT payload FROM revisions WHERE id=?1", [&key], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(old) = old {
                if serde_json::from_str::<RevisionDescriptor>(&old)? != descriptor {
                    return Err(Error::RevisionMismatch);
                }
            } else {
                conn.execute(
                    "INSERT INTO revisions(id,payload) VALUES (?1,?2)",
                    params![key, serde_json::to_string(&descriptor)?],
                )?;
            }
            conn.execute(
                "INSERT OR IGNORE INTO offline(revision,complete) VALUES (?1,0)",
                [key],
            )?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn set_retained_complete(
        &self,
        descriptor: RevisionDescriptor,
        complete: bool,
    ) -> Result<()> {
        self.run(move |conn| {
            let changed = conn.execute(
                "UPDATE offline SET complete=?2 WHERE revision=?1",
                params![revision_key(&descriptor)?, complete],
            )?;
            if changed != 1 {
                return Err(Error::NotFound);
            }
            Ok(())
        })
        .await
    }

    pub async fn retained(&self) -> Result<Vec<RevisionDescriptor>> {
        self.run(|conn| {
            let mut query = conn.prepare("SELECT r.payload FROM revisions r JOIN offline o ON r.id=o.revision WHERE o.complete=1 ORDER BY r.id")?;
            let rows = query.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect()
        }).await
    }
}

fn revision_key(descriptor: &RevisionDescriptor) -> Result<String> {
    Ok(serde_json::to_string(&(
        descriptor.asset(),
        descriptor.revision(),
    ))?)
}

fn checkpoint(tx: &Transaction<'_>, source: &SourceId) -> Result<Option<ScanCheckpoint>> {
    Ok(tx
        .query_row(
            "SELECT generation,cursor,complete,indexed FROM scans WHERE source=?1",
            [source.as_str()],
            |r| {
                Ok(ScanCheckpoint {
                    generation: unsigned_column(r, 0)?,
                    cursor: r.get(1)?,
                    complete: r.get(2)?,
                    indexed: unsigned_column(r, 3)?,
                })
            },
        )
        .optional()?)
}

fn open_connection(path: Option<&Path>) -> Result<Connection> {
    let mut conn = match path {
        None => Connection::open_in_memory()?,
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(path)?;
            Connection::open(path)?
        }
    };
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let id: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if id != 0 && id != APPLICATION_ID {
        return Err(Error::Invalid(
            "database belongs to another application".into(),
        ));
    }
    if version == 0 {
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables != 0 {
            return Err(Error::Invalid(
                "refusing to initialize a nonempty unknown database".into(),
            ));
        }
    } else if id != APPLICATION_ID {
        return Err(Error::Invalid(
            "catalog application identifier is missing".into(),
        ));
    }
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
    )?;
    if version == 0 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v1.sql"))?;
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    if version < 2 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v2.sql"))?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
    }
    if version < 3 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v3.sql"))?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
    }
    if version < 4 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v4.sql"))?;
        tx.pragma_update(None, "user_version", 4)?;
        tx.commit()?;
    }
    if version < 5 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v5.sql"))?;
        tx.pragma_update(None, "user_version", 5)?;
        tx.commit()?;
    }
    if version < 6 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v6.sql"))?;
        tx.pragma_update(None, "user_version", 6)?;
        tx.commit()?;
    }
    if version < 7 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v7.sql"))?;
        tx.pragma_update(None, "user_version", 7)?;
        tx.commit()?;
    }
    if version < 8 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v8.sql"))?;
        tx.pragma_update(None, "user_version", 8)?;
        tx.commit()?;
    }
    if version < 9 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v9.sql"))?;
        tx.pragma_update(None, "user_version", 9)?;
        tx.commit()?;
    }
    if version < 10 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v10.sql"))?;
        tx.pragma_update(None, "user_version", 10)?;
        tx.commit()?;
    }
    if version < 11 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v11.sql"))?;
        tx.pragma_update(None, "user_version", 11)?;
        tx.commit()?;
    }
    if version < 12 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v12.sql"))?;
        tx.pragma_update(None, "user_version", 12)?;
        tx.commit()?;
    }
    if version < 13 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v13.sql"))?;
        tx.pragma_update(None, "user_version", 13)?;
        tx.commit()?;
    }
    if version < 14 {
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("schema_v14.sql"))?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
    }
    Ok(conn)
}

fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::Invalid("counter exceeds SQLite integer range".into()))
}

fn unsigned_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
