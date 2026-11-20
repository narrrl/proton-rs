//! Account-scoped, versioned operations for gallery configuration.
use crate::{Error, Result, catalog::Catalog, library::AlbumSettings, model::*};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const VERSION: u32 = 1;
pub const RECORD_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlbumIdentity {
    pub volume: VolumeId,
    pub node: NodeId,
    pub photos_album: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Title(Option<String>),
    Description(Option<String>),
    Cover(Option<AssetKey>),
    Grouping(crate::library::Grouping),
    Order(SortOrder),
    Pinned(bool),
    Tags(Vec<String>),
    Hide,
    /// Only clears hides the author had already seen.
    Unhide(Vec<String>),
    Gallery(bool),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub version: u32,
    pub nonce: String,
    pub clock: u64,
    pub album: AlbumIdentity,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub id: String,
    pub document: String,
    pub pending: bool,
}

impl Record {
    pub fn decode(&self) -> Result<Operation> {
        if self.document.len() > RECORD_BYTES || hash(&self.document) != self.id {
            return Err(Error::Invalid(
                "Invalid gallery configuration record".into(),
            ));
        }
        let operation: Operation = serde_json::from_str(&self.document)?;
        if operation.version != VERSION {
            return Err(Error::Invalid(
                "Gallery settings need a newer app version".into(),
            ));
        }
        if operation.clock == 0
            || operation.changes.is_empty()
            || operation.clock >= i64::MAX as u64
        {
            return Err(Error::Invalid(
                "Invalid gallery configuration operation".into(),
            ));
        }
        Ok(operation)
    }
}

fn hash(document: &str) -> String {
    hex::encode(Sha256::digest(document.as_bytes()))
}

pub(crate) fn records(conn: &Connection, account: &AccountId) -> Result<Vec<Record>> {
    let mut stmt =
        conn.prepare("SELECT id,document,pending FROM gallery_configuration WHERE account=?1")?;
    let rows = stmt.query_map([account.as_str()], |row| {
        Ok(Record {
            id: row.get(0)?,
            document: row.get(1)?,
            pending: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn append(
    conn: &Connection,
    account: &AccountId,
    album: AlbumIdentity,
    changes: Vec<Change>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| Error::Invalid("Couldn’t generate a settings operation ID".into()))?;
    let clock: i64 = conn.query_row(
        "SELECT coalesce(max(json_extract(document,'$.clock')),0) FROM gallery_configuration WHERE account=?1",
        [account.as_str()],
        |r| r.get(0),
    )?;
    let operation = Operation {
        version: VERSION,
        nonce: hex::encode(nonce),
        clock: clock as u64 + 1,
        album,
        changes,
    };
    let document = serde_json::to_string(&operation)?;
    let id = hash(&document);
    Record {
        id: id.clone(),
        document: document.clone(),
        pending: true,
    }
    .decode()?;
    conn.execute(
        "INSERT INTO gallery_configuration(account,id,document,pending) VALUES(?1,?2,?3,1)",
        params![account.as_str(), id, document],
    )?;
    Ok(())
}

#[derive(Default)]
pub(crate) struct AlbumConfiguration {
    choices: Vec<Change>,
    hides: BTreeSet<String>,
    visibility: bool,
    pub gallery: Option<bool>,
}

impl AlbumConfiguration {
    pub fn apply(&self, mut settings: AlbumSettings) -> Result<AlbumSettings> {
        for change in &self.choices {
            match change {
                Change::Title(value) => settings.title.clone_from(value),
                Change::Description(value) => settings.description.clone_from(value),
                Change::Cover(value) => settings.cover.clone_from(value),
                Change::Grouping(value) => settings.grouping = *value,
                Change::Order(value) => settings.order = *value,
                Change::Pinned(value) => settings.pinned = *value,
                Change::Tags(value) => settings.tags.clone_from(value),
                _ => {}
            }
        }
        if self.visibility {
            settings.hidden = !self.hides.is_empty();
        }
        settings.normalized()
    }
}

pub(crate) fn merge(records: &[Record]) -> Result<BTreeMap<AlbumIdentity, AlbumConfiguration>> {
    let mut operations = records
        .iter()
        .map(|record| Ok((record.id.as_str(), record.decode()?)))
        .collect::<Result<Vec<_>>>()?;
    operations.sort_unstable_by(|(left_id, left), (right_id, right)| {
        (left.clock, left_id).cmp(&(right.clock, right_id))
    });
    let mut albums = BTreeMap::<_, AlbumConfiguration>::new();
    let mut cleared = BTreeMap::<AlbumIdentity, BTreeSet<String>>::new();
    for (id, operation) in operations {
        let album = albums.entry(operation.album.clone()).or_default();
        for change in operation.changes {
            match change {
                Change::Hide => {
                    album.visibility = true;
                    album.hides.insert(id.to_owned());
                }
                Change::Unhide(ids) => {
                    album.visibility = true;
                    cleared
                        .entry(operation.album.clone())
                        .or_default()
                        .extend(ids);
                }
                Change::Gallery(value) => album.gallery = Some(value),
                field => album.choices.push(field),
            }
        }
    }
    for (identity, clears) in cleared {
        if let Some(album) = albums.get_mut(&identity) {
            album.hides.retain(|id| !clears.contains(id));
        }
    }
    Ok(albums)
}

pub(crate) fn changes(
    conn: &Connection,
    account: &AccountId,
    identity: &AlbumIdentity,
    before: &AlbumSettings,
    after: &AlbumSettings,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    if before.title != after.title {
        changes.push(Change::Title(after.title.clone()));
    }
    if before.description != after.description {
        changes.push(Change::Description(after.description.clone()));
    }
    if before.cover != after.cover {
        changes.push(Change::Cover(after.cover.clone()));
    }
    if before.grouping != after.grouping {
        changes.push(Change::Grouping(after.grouping));
    }
    if before.order != after.order {
        changes.push(Change::Order(after.order));
    }
    if before.pinned != after.pinned {
        changes.push(Change::Pinned(after.pinned));
    }
    if before.tags != after.tags {
        changes.push(Change::Tags(after.tags.clone()));
    }
    if before.hidden != after.hidden {
        if after.hidden {
            changes.push(Change::Hide);
        } else {
            let hides = merge(&records(conn, account)?)?
                .remove(identity)
                .map(|album| album.hides)
                .unwrap_or_default();
            changes.push(Change::Unhide(hides.into_iter().collect()));
        }
    }
    Ok(changes)
}

impl Catalog {
    pub async fn configuration_records(&self, account: AccountId) -> Result<Vec<Record>> {
        self.run(move |conn| records(conn, &account)).await
    }
}

pub(crate) fn album_identity(
    conn: &Connection,
    source: &SourceId,
) -> Result<Option<(AccountId, AlbumIdentity)>> {
    use rusqlite::OptionalExtension;
    let row = conn.query_row(
        "SELECT s.payload,k.kind FROM sources s JOIN source_kinds k ON s.id=k.source WHERE s.id=?1 AND k.kind IN ('album','drive')",
        [source.as_str()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    row.map(|(payload, kind)| {
        let spec: SourceSpec = serde_json::from_str(&payload)?;
        Ok((
            spec.account,
            AlbumIdentity {
                volume: spec.volume,
                node: spec.root,
                photos_album: kind == "album",
            },
        ))
    })
    .transpose()
}

impl Catalog {
    /// Store a verified remote snapshot and apply settings for indexed albums in
    /// the same transaction. This also acknowledges identical outbox records.
    pub async fn merge_configuration(
        &self,
        account: AccountId,
        incoming: Vec<Record>,
    ) -> Result<Vec<(AlbumIdentity, bool)>> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for record in incoming {
                record.decode()?;
                tx.execute("INSERT INTO gallery_configuration(account,id,document,pending) VALUES(?1,?2,?3,0)
                    ON CONFLICT(account,id) DO UPDATE SET pending=0",
                    params![account.as_str(), record.id, record.document])?;
            }
            let merged = merge(&records(&tx, &account)?)?;
            let mut galleries = Vec::new();
            for (identity, configuration) in merged {
                if let Some(present) = configuration.gallery && !identity.photos_album {
                    galleries.push((identity.clone(), present));
                }
                let sources = {
                    let mut q = tx.prepare("SELECT s.id FROM sources s JOIN source_kinds k ON s.id=k.source
                        WHERE k.kind=?1 AND json_extract(s.payload,'$.account')=?2
                        AND json_extract(s.payload,'$.volume')=?3 AND json_extract(s.payload,'$.root')=?4")?;
                    let ids = q.query_map(params![if identity.photos_album { "album" } else { "drive" },
                        account.as_str(), identity.volume.as_str(), identity.node.as_str()], |r| r.get::<_, String>(0))?;
                    ids.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for id in sources {
                    let source = SourceId::new(id)?;
                    let before = crate::library::load_settings(&tx, source.as_str())?;
                    let after = configuration.apply(before.clone())?;
                    if after.cover.as_ref().is_some_and(|cover| cover.account != account) {
                        return Err(Error::Invalid("Cross-account gallery cover".into()));
                    }
                    // The catalog may not have crawled the chosen cover yet. The
                    // regular album query already falls back if it is missing.
                    if before != after {
                        crate::library::write_settings(&tx, &source, &after)?;
                    }
                }
            }
            tx.commit()?;
            Ok(galleries)
        }).await
    }
}

impl Catalog {
    /// Capture settings that predate the outbox. Only the designated importing
    /// client calls this before enrolling a new account configuration.
    pub async fn import_gallery_configuration(&self, account: AccountId) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let created = tx.execute(
                "INSERT OR IGNORE INTO gallery_configuration_imported(account) VALUES (?1)",
                [account.as_str()],
            )? != 0;
            if created {
                let sources = {
                    let mut q = tx.prepare("SELECT s.id FROM sources s JOIN source_kinds k ON s.id=k.source
                        WHERE k.kind IN ('album','drive') AND json_extract(s.payload,'$.account')=?1")?;
                    let ids = q.query_map([account.as_str()], |row| row.get::<_, String>(0))?;
                    ids.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let existing = merge(&records(&tx, &account)?)?;
                for source in sources {
                    let source = SourceId::new(source)?;
                    let Some((_, identity)) = album_identity(&tx, &source)? else { continue };
                    let after = crate::library::load_settings(&tx, source.as_str())?;
                    let before = existing.get(&identity).map(|album| album.apply(AlbumSettings::default()))
                        .transpose()?.unwrap_or_default();
                    let mut changes = changes(&tx, &account, &identity, &before, &after)?;
                    if !identity.photos_album && existing.get(&identity).is_none_or(|album| album.gallery.is_none()) {
                        let root = tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM album_nodes WHERE source=?1 AND gallery=?1 AND parent IS NULL)",
                            [source.as_str()], |row| row.get::<_, bool>(0),
                        )?;
                        if root { changes.push(Change::Gallery(true)); }
                    }
                    append(&tx, &account, identity, changes)?;
                }
            }
            tx.commit()?;
            Ok(())
        }).await
    }

    pub async fn set_gallery_configuration_membership(
        &self,
        account: AccountId,
        gallery: SourceId,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let (_, identity) = album_identity(&tx, &gallery)?
                .filter(|(owner, id)| owner == &account && !id.photos_album)
                .ok_or(Error::NotFound)?;
            append(&tx, &account, identity, vec![Change::Gallery(true)])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

impl Catalog {
    pub async fn hidden_configuration_albums(
        &self,
        account: AccountId,
    ) -> Result<Vec<AlbumIdentity>> {
        self.run(move |conn| {
            Ok(merge(&records(conn, &account)?)?
                .into_iter()
                .filter_map(|(id, settings)| (!settings.hides.is_empty()).then_some(id))
                .collect())
        })
        .await
    }

    pub async fn has_verified_gallery_visibility(&self, account: AccountId) -> Result<bool> {
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM gallery_visibility_verified WHERE account=?1)",
                [account.as_str()],
                |row| row.get(0),
            )?)
        })
        .await
    }

    pub async fn verify_gallery_visibility(&self, account: AccountId) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO gallery_visibility_verified(account) VALUES(?1)",
                [account.as_str()],
            )?;
            Ok(())
        })
        .await
    }
}

impl Catalog {
    pub async fn configuration_pending(&self, account: AccountId) -> Result<bool> {
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM gallery_configuration WHERE account=?1 AND pending=1)",
                [account.as_str()],
                |row| row.get(0),
            )?)
        })
        .await
    }

    pub async fn gallery_event_cursors(
        &self,
        account: AccountId,
    ) -> Result<BTreeMap<String, String>> {
        self.run(move |conn| {
            let mut query =
                conn.prepare("SELECT volume,cursor FROM gallery_event_cursors WHERE account=?1")?;
            let rows = query.query_map([account.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    /// The caller commits these only after retryable work has finished.
    pub async fn save_gallery_event_cursors(
        &self,
        account: AccountId,
        cursors: BTreeMap<String, String>,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for (volume, cursor) in cursors {
                tx.execute(
                    "INSERT INTO gallery_event_cursors(account,volume,cursor) VALUES(?1,?2,?3)
                    ON CONFLICT(account,volume) DO UPDATE SET cursor=excluded.cursor",
                    params![account.as_str(), volume, cursor],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SourceSpec {
        SourceSpec {
            id: SourceId::new("album").unwrap(),
            account: AccountId::new("account").unwrap(),
            volume: VolumeId::new("volume").unwrap(),
            root: NodeId::new("album-link").unwrap(),
            label: "Album".into(),
        }
    }

    #[tokio::test]
    async fn concurrent_hide_survives_unhide_and_presentation_edits_merge_by_field() {
        let first = Catalog::in_memory().await.unwrap();
        let second = Catalog::in_memory().await.unwrap();
        let album = spec();
        for catalog in [&first, &second] {
            catalog
                .sync_photo_album(album.clone(), vec![])
                .await
                .unwrap();
        }
        first
            .set_album_settings(
                album.id.clone(),
                AlbumSettings {
                    hidden: true,
                    pinned: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let first_hide = first
            .configuration_records(album.account.clone())
            .await
            .unwrap();
        second
            .set_album_settings(
                album.id.clone(),
                AlbumSettings {
                    hidden: true,
                    description: Some("Second edit".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        first
            .set_album_settings(
                album.id.clone(),
                AlbumSettings {
                    pinned: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        second
            .merge_configuration(album.account.clone(), first_hide)
            .await
            .unwrap();
        let first_records = first
            .configuration_records(album.account.clone())
            .await
            .unwrap();
        second
            .merge_configuration(album.account.clone(), first_records)
            .await
            .unwrap();
        let settings = second.album_settings(album.id.clone()).await.unwrap();
        assert!(settings.hidden && settings.pinned);
        assert_eq!(settings.description.as_deref(), Some("Second edit"));

        second
            .set_album_settings(
                album.id.clone(),
                AlbumSettings {
                    hidden: false,
                    ..settings
                },
            )
            .await
            .unwrap();
        let all = second
            .configuration_records(album.account.clone())
            .await
            .unwrap();
        first
            .merge_configuration(album.account.clone(), all)
            .await
            .unwrap();
        let settings = first.album_settings(album.id.clone()).await.unwrap();
        assert!(!settings.hidden);
        assert!(settings.pinned);
        assert_eq!(settings.description.as_deref(), Some("Second edit"));
        assert!(
            first
                .configuration_records(album.account.clone())
                .await
                .unwrap()
                .iter()
                .all(|record| !record.pending)
        );
        first.forget_account(album.account.clone()).await.unwrap();
        assert!(
            first
                .configuration_records(album.account)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
