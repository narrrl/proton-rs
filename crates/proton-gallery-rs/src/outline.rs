//! The outline of a whole view: every item's identity, time and shape, in
//! display order, without the full records.
//!
//! The grid plans its rows from the outline once, so its layout only changes
//! when the outline does: when media is added or removed. The records behind
//! the visible rows are read separately with [`Catalog::assets_by_key`].

use rusqlite::params_from_iter;
use rusqlite::types::Value;

use crate::catalog::Catalog;
use crate::model::*;
use crate::phone::VISIBLE;
use crate::{Error, Result};

/// The most items one outline holds. Far above any real library, and it bounds
/// the memory an outline can take (about 100 bytes an item).
pub const OUTLINE_MAX: usize = 1_000_000;

/// One item of an outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub key: AssetKey,
    pub sort_time: i64,
    /// The media's size in pixels, when known; 0 when not.
    pub width: u32,
    pub height: u32,
}

impl Catalog {
    /// The Photos timeline in the order of [`Catalog::timeline_window`].
    pub async fn timeline_outline(
        &self,
        photos: Option<SourceId>,
        account: Option<AccountId>,
    ) -> Result<Vec<OutlineEntry>> {
        self.timeline_outline_filtered(photos, account, false).await
    }

    pub async fn timeline_favorites_outline(
        &self,
        photos: Option<SourceId>,
        account: Option<AccountId>,
    ) -> Result<Vec<OutlineEntry>> {
        self.timeline_outline_filtered(photos, account, true).await
    }

    async fn timeline_outline_filtered(
        &self,
        photos: Option<SourceId>,
        account: Option<AccountId>,
        favorites: bool,
    ) -> Result<Vec<OutlineEntry>> {
        let filter = favorite_filter(favorites);
        let sql = format!(
            "SELECT m.account,m.volume,m.node,m.sort_time,s.width,s.height FROM memberships m
             LEFT JOIN media_shapes s ON (s.account,s.volume,s.node)=(m.account,m.volume,m.node)
             WHERE ({VISIBLE}) {filter}
             ORDER BY m.sort_time DESC,m.account DESC,m.volume DESC,m.node DESC LIMIT ?3"
        );
        let values = vec![
            photos.map_or(Value::Null, |s| s.as_str().to_owned().into()),
            account.map_or(Value::Null, |a| a.as_str().to_owned().into()),
            ((OUTLINE_MAX + 1) as i64).into(),
        ];
        self.run(move |conn| read(conn, &sql, values)).await
    }

    /// One source in `order`, as [`Catalog::window_ordered`] pages it.
    pub async fn source_outline(
        &self,
        source: SourceId,
        order: SortOrder,
    ) -> Result<Vec<OutlineEntry>> {
        self.source_outline_filtered(source, order, false, false)
            .await
    }

    pub async fn source_favorites_outline(
        &self,
        source: SourceId,
        order: SortOrder,
    ) -> Result<Vec<OutlineEntry>> {
        self.source_outline_filtered(source, order, true, false)
            .await
    }

    pub async fn visible_source_outline(
        &self,
        source: SourceId,
        order: SortOrder,
        favorites: bool,
    ) -> Result<Vec<OutlineEntry>> {
        self.source_outline_filtered(source, order, favorites, true)
            .await
    }

    async fn source_outline_filtered(
        &self,
        source: SourceId,
        order: SortOrder,
        favorites: bool,
        only_visible: bool,
    ) -> Result<Vec<OutlineEntry>> {
        let filter = favorite_filter(favorites);
        let (hidden, visibility) = if only_visible {
            (
                crate::library::HIDDEN_SOURCES,
                crate::library::VISIBLE_MEMBERSHIP,
            )
        } else {
            ("", "")
        };
        let first = if order.by_name() {
            "sort_name"
        } else {
            "sort_time"
        };
        let direction = if order.ascending() { "ASC" } else { "DESC" };
        let sql = format!(
            "{hidden} SELECT m.account,m.volume,m.node,m.sort_time,s.width,s.height FROM memberships m
             LEFT JOIN media_shapes s ON (s.account,s.volume,s.node)=(m.account,m.volume,m.node)
             WHERE m.source=?1 {filter} {visibility}
             ORDER BY m.{first} {direction},m.account {direction},m.volume {direction},m.node {direction} LIMIT ?2"
        );
        let values = vec![
            source.as_str().to_owned().into(),
            ((OUTLINE_MAX + 1) as i64).into(),
        ];
        self.run(move |conn| read(conn, &sql, values)).await
    }
}

fn favorite_filter(enabled: bool) -> &'static str {
    if enabled {
        "AND EXISTS(SELECT 1 FROM assets a WHERE (a.account,a.volume,a.node)=(m.account,m.volume,m.node) AND json_extract(a.payload,'$.favorite')=1)"
    } else {
        ""
    }
}

fn read(
    conn: &mut rusqlite::Connection,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<OutlineEntry>> {
    let mut query = conn.prepare_cached(sql)?;
    let rows = query.query_map(params_from_iter(values), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (account, volume, node, sort_time, width, height) = row?;
        if entries.len() == OUTLINE_MAX {
            return Err(Error::Invalid("the view has too many items".into()));
        }
        let size = |v: Option<i64>| v.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let (width, height) = (size(width), size(height));
        let known = width > 0 && height > 0;
        entries.push(OutlineEntry {
            key: AssetKey {
                account: AccountId::new(account)?,
                volume: VolumeId::new(volume)?,
                node: NodeId::new(node)?,
            },
            sort_time,
            width: if known { width } else { 0 },
            height: if known { height } else { 0 },
        });
    }
    Ok(entries)
}
