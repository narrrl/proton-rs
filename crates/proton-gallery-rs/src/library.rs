//! Photos timeline and folder galleries are different namespaces. Every Drive
//! folder is an album, and can contain both direct media and nested albums.
//! Proton Photos albums share the album tables as single-level galleries of
//! kind 'album' (see `photo_albums`), so settings, covers and paging work the
//! same for both.
use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

// Hide media inherited from a hidden folder even when the same source is
// available through a second gallery or a public album.
pub(crate) const HIDDEN_SOURCES: &str = "WITH RECURSIVE
    hidden_folders(gallery,source) AS (
        SELECT n.gallery,n.source FROM album_nodes n JOIN album_settings a ON a.source=n.source WHERE a.hidden=1
        UNION SELECT n.gallery,n.source FROM album_nodes n JOIN hidden_folders h ON (n.gallery,n.parent)=(h.gallery,h.source)
    ), hidden_sources(source) AS (SELECT DISTINCT source FROM hidden_folders)";
pub(crate) const VISIBLE_MEMBERSHIP: &str = "AND NOT EXISTS (
    SELECT 1 FROM memberships h JOIN hidden_sources x ON x.source=h.source
    WHERE (h.account,h.volume,h.node)=(m.account,m.volume,m.node))";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Photos,
    Drive,
    Device,
    /// A Proton Photos album.
    Album,
    Other,
}
impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photos => "photos",
            Self::Drive => "drive",
            Self::Device => "device",
            Self::Album => "album",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Album {
    pub gallery: SourceId,
    pub source: SourceSpec,
    pub parent: Option<SourceId>,
    #[serde(skip)]
    pub photos_album: bool,
    pub direct_items: u64,
    pub child_albums: u64,
    pub settings: AlbumSettings,
    /// The chosen cover while it is still in the album, otherwise the newest
    /// item of the album or, for an album of albums, of its subalbums.
    pub cover: Option<Asset>,
}

/// How a media grid is divided into headed sections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    #[default]
    Day,
    Month,
    Year,
    None,
}
impl Grouping {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
            Self::Year => "year",
            Self::None => "none",
        }
    }
    fn parse(value: &str) -> Self {
        match value {
            "month" => Self::Month,
            "year" => Self::Year,
            "none" => Self::None,
            _ => Self::Day,
        }
    }
}

/// Local presentation choices for one source. Never written back to Proton.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumSettings {
    /// Display name; `None` shows the folder name.
    pub title: Option<String>,
    pub description: Option<String>,
    /// Chosen cover; `None` uses the newest item.
    pub cover: Option<AssetKey>,
    pub grouping: Grouping,
    /// Date headings only apply to the date orders.
    pub order: SortOrder,
    /// Listed before other albums.
    pub pinned: bool,
    /// Left out of album lists unless hidden albums are shown.
    pub hidden: bool,
    pub tags: Vec<String>,
}

pub const MAX_TAGS: usize = 20;

impl AlbumSettings {
    /// Trim text, drop empty values, and deduplicate tags without regard to case.
    pub fn normalized(mut self) -> Result<Self> {
        fn text(value: Option<String>, limit: usize, what: &str) -> Result<Option<String>> {
            let Some(value) = value else { return Ok(None) };
            let value = value.trim();
            if value.chars().count() > limit {
                return Err(Error::Invalid(format!(
                    "{what} is longer than {limit} characters"
                )));
            }
            Ok((!value.is_empty()).then(|| value.to_owned()))
        }
        self.title = text(self.title, 200, "album name")?;
        self.description = text(self.description, 2000, "album description")?;
        let mut tags: Vec<String> = Vec::new();
        for tag in self.tags {
            let tag = tag.split_whitespace().collect::<Vec<_>>().join(" ");
            if tag.is_empty() || tags.iter().any(|t| t.to_lowercase() == tag.to_lowercase()) {
                continue;
            }
            if tag.chars().count() > 40 {
                return Err(Error::Invalid("a tag is longer than 40 characters".into()));
            }
            tags.push(tag);
        }
        if tags.len() > MAX_TAGS {
            return Err(Error::Invalid(format!(
                "an album can have at most {MAX_TAGS} tags"
            )));
        }
        self.tags = tags;
        Ok(self)
    }
}

pub(crate) fn load_settings(conn: &rusqlite::Connection, source: &str) -> Result<AlbumSettings> {
    let row = conn
        .query_row(
            "SELECT title,description,cover_account,cover_volume,cover_node,grouping,sort_order,pinned,hidden FROM album_settings WHERE source=?1",
            [source],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, bool>(7)?,
                    r.get::<_, bool>(8)?,
                ))
            },
        )
        .optional()?;
    let mut settings = AlbumSettings::default();
    if let Some((title, description, account, volume, node, grouping, order, pinned, hidden)) = row
    {
        let cover = match (account, volume, node) {
            (Some(account), Some(volume), Some(node)) => Some(AssetKey {
                account: AccountId::new(account)?,
                volume: VolumeId::new(volume)?,
                node: NodeId::new(node)?,
            }),
            _ => None,
        };
        settings = AlbumSettings {
            title,
            description,
            cover,
            grouping: Grouping::parse(&grouping),
            order: SortOrder::parse(&order),
            pinned,
            hidden,
            tags: vec![],
        };
    }
    let mut q = conn
        .prepare_cached("SELECT tag FROM album_tags WHERE source=?1 ORDER BY tag COLLATE NOCASE")?;
    settings.tags = q
        .query_map([source], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(settings)
}

fn has_hidden_ancestor(conn: &rusqlite::Connection, gallery: &str, source: &str) -> Result<bool> {
    Ok(conn.query_row(
        "WITH RECURSIVE ancestors(source) AS (
             SELECT parent FROM album_nodes WHERE gallery=?1 AND source=?2
             UNION
             SELECT n.parent FROM album_nodes n JOIN ancestors a ON n.source=a.source WHERE n.gallery=?1
         )
         SELECT EXISTS(SELECT 1 FROM ancestors a JOIN album_settings s ON s.source=a.source WHERE s.hidden=1)",
        params![gallery, source],
        |row| row.get(0),
    )?)
}

/// The chosen cover if it is still in this album or below it in `gallery`,
/// else the first direct item in the album's order, else the first item of
/// its subalbums in that order.
fn resolve_cover(
    conn: &rusqlite::Connection,
    gallery: &str,
    source: &str,
    chosen: Option<&AssetKey>,
    order: SortOrder,
    album_hidden: bool,
) -> Result<Option<Asset>> {
    const TREE: &str = "tree(source) AS (SELECT ?1 UNION SELECT c.source FROM album_nodes c JOIN tree t ON c.parent=t.source WHERE c.gallery=?2)";
    let (hidden, tree, visibility) = if album_hidden {
        ("", format!("WITH RECURSIVE {TREE}"), "")
    } else {
        (
            HIDDEN_SOURCES,
            format!("{HIDDEN_SOURCES}, {TREE}"),
            VISIBLE_MEMBERSHIP,
        )
    };
    if let Some(key) = chosen {
        let payload: Option<String> = conn
            .query_row(
                &format!("{tree} SELECT a.payload FROM assets a WHERE (a.account,a.volume,a.node)=(?3,?4,?5) AND EXISTS(SELECT 1 FROM memberships m WHERE m.source IN tree AND (m.account,m.volume,m.node)=(?3,?4,?5) {visibility})"),
                params![source, gallery, key.account.as_str(), key.volume.as_str(), key.node.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(payload) = payload {
            return Ok(Some(serde_json::from_str(&payload)?));
        }
    }
    // The cover follows the album's own order: the album opens with it.
    let first = if order.by_name() {
        "sort_name"
    } else {
        "sort_time"
    };
    let order = if order.ascending() { "ASC" } else { "DESC" };
    let direct: Option<String> = conn
        .query_row(
            &format!("{hidden} SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node) WHERE m.source=?1 {visibility} ORDER BY m.{first} {order},m.account {order},m.volume {order},m.node {order} LIMIT 1"),
            [source],
            |r| r.get(0),
        )
        .optional()?;
    let payload = match direct {
        Some(payload) => Some(payload),
        None => conn
            .query_row(
                &format!("{tree} SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node) WHERE m.source IN tree {visibility} ORDER BY m.{first} {order},m.account {order},m.volume {order},m.node {order} LIMIT 1"),
                params![source, gallery],
                |r| r.get(0),
            )
            .optional()?,
    };
    payload.map(|p| Ok(serde_json::from_str(&p)?)).transpose()
}

pub(crate) fn write_settings(
    tx: &rusqlite::Connection,
    source: &SourceId,
    settings: &AlbumSettings,
) -> Result<()> {
    let cover = settings.cover.as_ref();
    tx.execute(
        "INSERT INTO album_settings(source,title,description,cover_account,cover_volume,cover_node,grouping,sort_order,pinned,hidden)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(source) DO UPDATE SET title=excluded.title,description=excluded.description,cover_account=excluded.cover_account,
         cover_volume=excluded.cover_volume,cover_node=excluded.cover_node,grouping=excluded.grouping,sort_order=excluded.sort_order,
         pinned=excluded.pinned,hidden=excluded.hidden",
        params![
            source.as_str(), settings.title, settings.description,
            cover.map(|c| c.account.as_str()), cover.map(|c| c.volume.as_str()), cover.map(|c| c.node.as_str()),
            settings.grouping.as_str(), settings.order.as_str(), settings.pinned, settings.hidden
        ],
    )?;
    tx.execute("DELETE FROM album_tags WHERE source=?1", [source.as_str()])?;
    for tag in &settings.tags {
        tx.execute(
            "INSERT INTO album_tags(source,tag) VALUES (?1,?2)",
            params![source.as_str(), tag],
        )?;
    }
    Ok(())
}

impl Catalog {
    pub async fn source_count(&self, source: SourceId) -> Result<u64> {
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM memberships WHERE source=?1",
                [source.as_str()],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
        .await
    }
    pub async fn set_source_kind(&self, source: SourceId, kind: SourceKind) -> Result<()> {
        self.run(move |conn| {
            let old: Option<String> = conn.query_row("SELECT kind FROM source_kinds WHERE source=?1",[source.as_str()],|r|r.get(0)).optional()?;
            if old.as_deref().is_some_and(|old| old!="other" && old!=kind.as_str()) {
                return Err(Error::Invalid("source cannot change between Photos, Drive, and device media".into()));
            }
            conn.execute("INSERT INTO source_kinds(source,kind) VALUES (?1,?2) ON CONFLICT(source) DO UPDATE SET kind=excluded.kind",params![source.as_str(),kind.as_str()])?;
            Ok(())
        }).await
    }

    pub async fn sources_of_kind(&self, kind: SourceKind) -> Result<Vec<SourceSpec>> {
        self.run(move |conn| {
            let mut q=conn.prepare("SELECT s.payload FROM sources s JOIN source_kinds k ON s.id=k.source WHERE k.kind=?1 ORDER BY lower(json_extract(s.payload,'$.label')),s.id LIMIT 250")?;
            q.query_map([kind.as_str()],|r|r.get::<_,String>(0))?.map(|r|Ok(serde_json::from_str(&r?)?)).collect()
        }).await
    }

    /// Filter in SQL before applying the source window, so another account
    /// cannot consume or leak through the current account's result window.
    pub async fn account_sources(
        &self,
        kind: SourceKind,
        account: AccountId,
    ) -> Result<Vec<SourceSpec>> {
        self.run(move |conn| {
            let mut q=conn.prepare("SELECT s.payload FROM sources s JOIN source_kinds k ON s.id=k.source WHERE k.kind=?1 AND json_extract(s.payload,'$.account')=?2 ORDER BY s.id LIMIT 250")?;
            q.query_map(params![kind.as_str(),account.as_str()],|r|r.get::<_,String>(0))?.map(|r|Ok(serde_json::from_str(&r?)?)).collect()
        }).await
    }
    pub async fn source_belongs_to(&self, source: SourceId, account: AccountId) -> Result<bool> {
        self.run(move |conn| Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sources WHERE id=?1 AND json_extract(payload,'$.account')=?2)",params![source.as_str(),account.as_str()],|r|r.get(0))?)).await
    }

    pub async fn account_source(
        &self,
        kind: SourceKind,
        account: AccountId,
        id: SourceId,
    ) -> Result<Option<SourceSpec>> {
        self.run(move |conn| {
            let payload = conn
                .query_row(
                    "SELECT s.payload FROM sources s JOIN source_kinds k ON k.source=s.id
                 WHERE s.id=?1 AND k.kind=?2 AND json_extract(s.payload,'$.account')=?3",
                    params![id.as_str(), kind.as_str(), account.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            Ok(payload
                .map(|payload| serde_json::from_str(&payload))
                .transpose()?)
        })
        .await
    }

    /// Register a stable folder in one selected gallery. Overlapping gallery
    /// roots share media/source identity, but retain independent parent edges.
    pub async fn register_album(
        &self,
        gallery: SourceId,
        source: SourceSpec,
        parent: Option<SourceId>,
    ) -> Result<()> {
        self.register_source(source.clone()).await?;
        self.set_source_kind(source.id.clone(), SourceKind::Drive)
            .await?;
        self.run(move |conn| {
            let tx=conn.transaction()?;
            if (parent.is_none()) != (source.id==gallery) || parent.as_ref()==Some(&source.id) {
                return Err(Error::Invalid("invalid album root or parent".into()));
            }
            let root: SourceSpec=serde_json::from_str(&tx.query_row("SELECT payload FROM sources WHERE id=?1",[gallery.as_str()],|r|r.get::<_,String>(0))?)?;
            if root.account!=source.account || root.volume!=source.volume {
                return Err(Error::Invalid("album belongs to another account or volume".into()));
            }
            if let Some(parent)=&parent {
                let mut at=Some(parent.to_string());
                let mut depth=0;
                while let Some(id)=at {
                    if id==source.id.as_str() { return Err(Error::Invalid("album cycle".into())); }
                    depth+=1;
                    if depth>128 { return Err(Error::Invalid("album nesting exceeds 128 folders".into())); }
                    at=tx.query_row("SELECT parent FROM album_nodes WHERE gallery=?1 AND source=?2",params![gallery.as_str(),id],|r|r.get::<_,Option<String>>(0)).optional()?.ok_or(Error::NotFound)?;
                }
            }
            tx.execute("INSERT INTO album_nodes(gallery,source,parent) VALUES (?1,?2,?3) ON CONFLICT(gallery,source) DO UPDATE SET parent=excluded.parent",
                params![gallery.as_str(),source.id.as_str(),parent.as_ref().map(SourceId::as_str)])?;
            tx.commit()?;
            Ok(())
        }).await
    }

    /// Roots when both arguments are None; direct child albums otherwise.
    pub async fn albums(
        &self,
        gallery: Option<SourceId>,
        parent: Option<SourceId>,
    ) -> Result<Vec<Album>> {
        self.albums_scoped(gallery, parent, None).await
    }

    pub async fn albums_scoped(
        &self,
        gallery: Option<SourceId>,
        parent: Option<SourceId>,
        account: Option<AccountId>,
    ) -> Result<Vec<Album>> {
        if gallery.is_none() != parent.is_none() {
            return Err(Error::Invalid(
                "album query needs both gallery and parent".into(),
            ));
        }
        self.query_albums(
            "((?1 IS NULL AND n.parent IS NULL AND NOT EXISTS(SELECT 1 FROM source_kinds k WHERE k.source=n.source AND k.kind='album'))
              OR (n.gallery=?1 AND n.parent=?2))",
            vec![
                gallery.map(String::from).into(),
                parent.map(String::from).into(),
            ],
            account,
        )
        .await
    }

    /// Entry points into private Drive galleries, including folders beneath
    /// public parents. Private descendants are reached through these roots.
    pub async fn hidden_drive_albums(&self, account: AccountId) -> Result<Vec<Album>> {
        self.query_albums(
            "x.hidden=1 AND ?1 IS NULL AND ?2 IS NULL
             AND NOT EXISTS(SELECT 1 FROM source_kinds k WHERE k.source=n.source AND k.kind='album')
             AND NOT EXISTS(
                 WITH RECURSIVE ancestors(source) AS (
                     SELECT n.parent
                     UNION
                     SELECT p.parent FROM album_nodes p JOIN ancestors a ON p.source=a.source WHERE p.gallery=n.gallery
                 ) SELECT 1 FROM ancestors a JOIN album_settings s ON s.source=a.source WHERE s.hidden=1
             )",
            vec![rusqlite::types::Value::Null; 2],
            Some(account),
        )
        .await
    }

    pub async fn drive_album_is_private(
        &self,
        gallery: SourceId,
        source: SourceId,
    ) -> Result<bool> {
        self.run(move |conn| {
            Ok(load_settings(conn, source.as_str())?.hidden
                || has_hidden_ancestor(conn, gallery.as_str(), source.as_str())?)
        })
        .await
    }

    /// Albums with this tag that can be opened directly from the tag results.
    /// Descendants of hidden albums are reached through the unlocked parent.
    pub async fn tagged_albums(&self, tag: String, account: AccountId) -> Result<Vec<Album>> {
        self.query_albums(
            "EXISTS(SELECT 1 FROM album_tags t WHERE t.source=n.source AND t.tag=?1 COLLATE NOCASE) AND ?2 IS NULL
             AND NOT EXISTS(
                 WITH RECURSIVE ancestors(source) AS (
                     SELECT n.parent
                     UNION
                     SELECT p.parent FROM album_nodes p JOIN ancestors a ON p.source=a.source WHERE p.gallery=n.gallery
                 ) SELECT 1 FROM ancestors a JOIN album_settings s ON s.source=a.source WHERE s.hidden=1
             )",
            vec![Some(tag).into(), rusqlite::types::Value::Null],
            Some(account),
        )
        .await
    }

    /// Distinct tags in use by `account`'s albums, with how many albums use each.
    pub async fn album_tags(&self, account: AccountId) -> Result<Vec<(String, u64)>> {
        self.run(move |conn| {
            let mut q = conn.prepare(
                "SELECT min(t.tag),count(DISTINCT t.source) FROM album_tags t JOIN sources s ON s.id=t.source
                 WHERE json_extract(s.payload,'$.account')=?1 AND EXISTS(SELECT 1 FROM album_nodes n WHERE n.source=t.source)
                 GROUP BY t.tag COLLATE NOCASE ORDER BY min(t.tag) COLLATE NOCASE LIMIT 200",
            )?;
            q.query_map([account.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
                .map(|r| Ok(r?))
                .collect()
        })
        .await
    }

    /// Account-owned items excluded from public gallery views, including
    /// members of descendants of a hidden Drive album.
    pub async fn hidden_assets(&self, account: AccountId) -> Result<Vec<(String, String)>> {
        self.run(move |conn| {
            let mut query = conn.prepare(&format!(
                "{HIDDEN_SOURCES} SELECT DISTINCT m.volume, m.node FROM memberships m
                 JOIN hidden_sources h ON h.source=m.source WHERE m.account=?1"
            ))?;
            Ok(query
                .query_map([account.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
    }

    pub async fn album_settings(&self, source: SourceId) -> Result<AlbumSettings> {
        self.run(move |conn| load_settings(conn, source.as_str()))
            .await
    }

    /// Replace a source's presentation settings. A cover must be an item of
    /// the album or one of its subalbums.
    pub async fn set_album_settings(
        &self,
        source: SourceId,
        settings: AlbumSettings,
    ) -> Result<AlbumSettings> {
        self.set_album_settings_at(None, source, settings).await
    }

    /// Apply the settings from a specific placement of the album in a gallery.
    pub async fn set_album_settings_at(
        &self,
        gallery: Option<SourceId>,
        source: SourceId,
        settings: AlbumSettings,
    ) -> Result<AlbumSettings> {
        let settings = settings.normalized()?;
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sources WHERE id=?1)", [source.as_str()], |r| r.get(0))?;
            if !exists {
                return Err(Error::NotFound);
            }
            // Hiding takes media out of the timeline and the photo picker,
            // so the timeline itself and the phone cannot be hidden.
            let shared: bool = tx.query_row(
                "SELECT ?1='phone' OR EXISTS(SELECT 1 FROM source_kinds WHERE source=?1 AND kind='photos')",
                [source.as_str()],
                |r| r.get(0),
            )?;
            if settings.hidden && shared {
                return Err(Error::Invalid("the timeline cannot be hidden".into()));
            }
            let before = load_settings(&tx, source.as_str())?;
            // The cover must be allowed under the proposed visibility. The
            // transaction rolls back the settings if validation fails.
            write_settings(&tx, &source, &settings)?;
            if let Some(cover) = &settings.cover {
                let gallery = match gallery {
                    Some(gallery) => Some(gallery.to_string()),
                    None => tx.query_row(
                        "SELECT gallery FROM album_nodes WHERE source=?1 ORDER BY gallery LIMIT 1",
                        [source.as_str()],
                        |row| row.get::<_, String>(0),
                    ).optional()?,
                };
                let member: bool = tx.query_row(
                    "WITH RECURSIVE tree(source) AS (
                         SELECT ?1 UNION SELECT c.source FROM album_nodes c JOIN tree t ON c.parent=t.source
                         WHERE (?5 IS NULL OR c.gallery=?5)
                     )
                     SELECT EXISTS(SELECT 1 FROM memberships m WHERE m.source IN tree AND (m.account,m.volume,m.node)=(?2,?3,?4))",
                    params![source.as_str(), cover.account.as_str(), cover.volume.as_str(), cover.node.as_str(), gallery],
                    |r| r.get(0),
                )?;
                if !member {
                    return Err(Error::Invalid("the cover must be a photo or video from this album".into()));
                }
                if let Some(gallery) = gallery
                    && !settings.hidden
                    && !has_hidden_ancestor(&tx, &gallery, source.as_str())?
                    && !resolve_cover(&tx, &gallery, source.as_str(), Some(cover), settings.order, false)?
                        .is_some_and(|asset| asset.key == *cover)
                {
                    return Err(Error::Invalid("a hidden item cannot be the cover of a visible album".into()));
                }
            }
            if let Some((account, identity)) = crate::configuration::album_identity(&tx, &source)? {
                let changes = crate::configuration::changes(&tx, &account, &identity, &before, &settings)?;
                crate::configuration::append(&tx, &account, identity, changes)?;
            }
            let saved = load_settings(&tx, source.as_str())?;
            tx.commit()?;
            Ok(saved)
        })
        .await
    }

    /// Forget a gallery's album tree. Media and settings stay: another
    /// gallery can still contain the same folders.
    pub async fn remove_gallery(&self, gallery: SourceId, account: AccountId) -> Result<()> {
        self.forget_gallery_tree(gallery, account, true).await
    }

    /// Remove a gallery selected on another device, without publishing an echo.
    pub async fn apply_gallery_removal(&self, gallery: SourceId, account: AccountId) -> Result<()> {
        self.forget_gallery_tree(gallery, account, false).await
    }

    async fn forget_gallery_tree(
        &self,
        gallery: SourceId,
        account: AccountId,
        record: bool,
    ) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.transaction()?;
            let owned: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM album_nodes n JOIN sources s ON s.id=n.source WHERE n.gallery=?1 AND n.source=?1 AND json_extract(s.payload,'$.account')=?2)",
                params![gallery.as_str(), account.as_str()],
                |r| r.get(0),
            )?;
            if !owned {
                return if record { Err(Error::NotFound) } else { Ok(()) };
            }
            if record && let Some((owner, identity)) = crate::configuration::album_identity(&tx, &gallery)? {
                crate::configuration::append(&tx, &owner, identity, vec![crate::configuration::Change::Gallery(false)])?;
            }
            // Children before parents: the parent edge is a foreign key.
            tx.execute_batch("PRAGMA defer_foreign_keys=ON")?;
            tx.execute("DELETE FROM album_nodes WHERE gallery=?1", [gallery.as_str()])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn query_albums(
        &self,
        filter: &'static str,
        values: Vec<rusqlite::types::Value>,
        account: Option<AccountId>,
    ) -> Result<Vec<Album>> {
        self.run(move |conn| {
            let mut query = conn.prepare(&format!(
                "SELECT n.gallery,s.payload,n.parent,
                (SELECT count(*) FROM memberships m WHERE m.source=n.source),
                (SELECT count(*) FROM album_nodes c WHERE c.gallery=n.gallery AND c.parent=n.source),
                EXISTS(SELECT 1 FROM source_kinds k WHERE k.source=n.source AND k.kind='album')
                FROM album_nodes n JOIN sources s ON s.id=n.source LEFT JOIN album_settings x ON x.source=n.source
                WHERE {filter} AND (?3 IS NULL OR json_extract(s.payload,'$.account')=?3)
                ORDER BY coalesce(x.pinned,0) DESC,lower(coalesce(x.title,json_extract(s.payload,'$.label'))),s.id,n.gallery LIMIT 250",
            ))?;
            let rows: Vec<_> = query
                .query_map(
                    params![values[0], values[1], account.as_ref().map(AccountId::as_str)],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, bool>(5)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
            rows.into_iter()
                .map(|(gallery, payload, parent, items, children, photos_album)| {
                    let source: SourceSpec = serde_json::from_str(&payload)?;
                    let settings = load_settings(conn, source.id.as_str())?;
                    let private = settings.hidden || has_hidden_ancestor(conn, &gallery, source.id.as_str())?;
                    let cover = resolve_cover(conn, &gallery, source.id.as_str(), settings.cover.as_ref(), settings.order, private)?;
                    Ok(Album {
                        gallery: SourceId::new(gallery)?,
                        source,
                        parent: parent.map(SourceId::new).transpose()?,
                        photos_album,
                        direct_items: items as u64,
                        child_albums: children as u64,
                        settings,
                        cover,
                    })
                })
                .collect()
        })
        .await
    }

    pub async fn album_exists(&self, gallery: SourceId, source: SourceId) -> Result<bool> {
        self.run(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM album_nodes WHERE gallery=?1 AND source=?2)",
                params![gallery.as_str(), source.as_str()],
                |r| r.get(0),
            )?)
        })
        .await
    }

    pub async fn remember_shape(&self, asset: AssetKey, width: u32, height: u32) -> Result<()> {
        if !(1..=1_000_000).contains(&width) || !(1..=1_000_000).contains(&height) {
            return Err(Error::Invalid("invalid media dimensions".into()));
        }
        self.run(move |conn| {
            conn.execute("INSERT INTO media_shapes(account,volume,node,width,height) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(account,volume,node) DO UPDATE SET width=excluded.width,height=excluded.height",
                params![asset.account.as_str(),asset.volume.as_str(),asset.node.as_str(),width,height])?;
            Ok(())
        }).await
    }

    /// Mark media as a favorite or not, after Proton accepted the change.
    pub async fn set_favorite(&self, keys: Vec<AssetKey>, favorite: bool) -> Result<()> {
        if keys.len() > 5_000 {
            return Err(Error::Invalid("too many items to change at once".into()));
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut read = tx.prepare_cached(
                    "SELECT payload FROM assets WHERE (account,volume,node)=(?1,?2,?3)",
                )?;
                let mut write = tx.prepare_cached(
                    "UPDATE assets SET payload=?4 WHERE (account,volume,node)=(?1,?2,?3)",
                )?;
                for key in &keys {
                    let id = params![key.account.as_str(), key.volume.as_str(), key.node.as_str()];
                    let Some(payload) = read.query_row(id, |r| r.get::<_, String>(0)).optional()?
                    else {
                        continue;
                    };
                    let mut asset: Asset = serde_json::from_str(&payload)?;
                    if asset.favorite == favorite {
                        continue;
                    }
                    asset.favorite = favorite;
                    write.execute(params![
                        key.account.as_str(),
                        key.volume.as_str(),
                        key.node.as_str(),
                        serde_json::to_string(&asset)?
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn aspect_ratios(&self, keys: Vec<AssetKey>) -> Result<Vec<Option<f64>>> {
        if keys.len() > PageSize::MAX {
            return Err(Error::Invalid("too many media shapes requested".into()));
        }
        self.run(move |conn| {
            let mut q=conn.prepare_cached("SELECT CAST(width AS REAL)/height FROM media_shapes WHERE (account,volume,node)=(?1,?2,?3)")?;
            keys.into_iter().map(|key|Ok(q.query_row(params![key.account.as_str(),key.volume.as_str(),key.node.as_str()],|r|r.get(0)).optional()?)).collect()
        }).await
    }
}
