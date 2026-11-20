//! Proton Photos for the system photo picker.
//!
//! Android's picker keeps its own copy of a cloud library and asks for what
//! changed since the last generation it saw. The catalog counts changes to
//! Photos-source members in `picker_changes` (see schema v7); members that
//! leave the source stay there marked deleted (schema v8). Members in a
//! hidden album are reported as deleted while it is hidden (schema v10).
use rusqlite::{OptionalExtension, params};

use crate::catalog::Catalog;
use crate::model::*;
use crate::phone::not_hidden;
use crate::{Error, Result};

/// The picker's view of the catalog: a random identity and the last change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerState {
    pub collection: String,
    pub generation: u64,
}

/// One Proton photo or video as the picker lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerEntry {
    pub asset: Asset,
    pub generation: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The phone's media identifier when the same file is on this phone.
    pub phone: Option<String>,
}

/// The largest page the picker gets at once.
pub const PICKER_PAGE_MAX: u32 = 1000;

impl Catalog {
    pub async fn picker_state(&self) -> Result<PickerState> {
        self.run(|conn| {
            let (collection, generation): (String, i64) = conn.query_row(
                "SELECT collection,generation FROM picker_state WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(PickerState {
                collection,
                generation: u64::try_from(generation)
                    .map_err(|_| Error::Invalid("negative picker generation".into()))?,
            })
        })
        .await
    }

    /// Members of `source` changed after generation `after`, oldest change first.
    pub async fn picker_changes(
        &self,
        source: SourceId,
        after: u64,
        limit: u32,
    ) -> Result<Vec<PickerEntry>> {
        let after =
            i64::try_from(after).map_err(|_| Error::Invalid("generation out of range".into()))?;
        let limit = limit.clamp(1, PICKER_PAGE_MAX);
        self.run(move |conn| {
            let mut query = conn.prepare_cached(&format!(
                "SELECT c.generation,a.payload,s.width,s.height,
                    (SELECT b.node FROM phone_backup b WHERE b.account=c.account
                        AND b.remote_volume=c.volume AND b.remote_node=c.node LIMIT 1)
                 FROM picker_changes c
                 JOIN memberships m ON m.source=?1 AND (m.account,m.volume,m.node)=(c.account,c.volume,c.node)
                 JOIN assets a ON (a.account,a.volume,a.node)=(c.account,c.volume,c.node)
                 LEFT JOIN media_shapes s ON (s.account,s.volume,s.node)=(c.account,c.volume,c.node)
                 WHERE c.generation>?2 AND {} ORDER BY c.generation LIMIT ?3",
                not_hidden("c")
            ))?;
            let rows = query.query_map(params![source.as_str(), after, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<u32>>(2)?,
                    r.get::<_, Option<u32>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            rows.map(|row| {
                let (generation, payload, width, height, phone) = row?;
                Ok(PickerEntry {
                    asset: serde_json::from_str(&payload)?,
                    generation: u64::try_from(generation)
                        .map_err(|_| Error::Invalid("negative picker generation".into()))?,
                    width,
                    height,
                    phone,
                })
            })
            .collect()
        })
        .await
    }

    /// Media of `source` removed after generation `after`, oldest first:
    /// the node and the generation of its removal.
    pub async fn picker_deletions(
        &self,
        source: SourceSpec,
        after: u64,
        limit: u32,
    ) -> Result<Vec<(NodeId, u64)>> {
        let after =
            i64::try_from(after).map_err(|_| Error::Invalid("generation out of range".into()))?;
        let limit = limit.clamp(1, PICKER_PAGE_MAX);
        self.run(move |conn| {
            let mut query = conn.prepare_cached(&format!(
                "SELECT c.node,c.generation FROM picker_changes c
                 WHERE (c.deleted=1 OR NOT {}) AND c.generation>?1 AND c.account=?2 AND c.volume=?3
                 ORDER BY c.generation LIMIT ?4",
                not_hidden("c")
            ))?;
            let rows = query.query_map(
                params![
                    after,
                    source.account.as_str(),
                    source.volume.as_str(),
                    limit
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )?;
            rows.map(|row| {
                let (node, generation) = row?;
                Ok((
                    NodeId::new(node)?,
                    u64::try_from(generation)
                        .map_err(|_| Error::Invalid("negative picker generation".into()))?,
                ))
            })
            .collect()
        })
        .await
    }

    /// The member of `source` with this node, if any.
    pub async fn picker_asset(&self, source: SourceSpec, node: NodeId) -> Result<Option<Asset>> {
        self.run(move |conn| {
            let payload: Option<String> = conn
                .query_row(
                    &format!("SELECT a.payload FROM memberships m JOIN assets a USING(account,volume,node)
                     WHERE m.source=?1 AND m.account=?2 AND m.volume=?3 AND m.node=?4 AND {}", not_hidden("m")),
                    params![
                        source.id.as_str(),
                        source.account.as_str(),
                        source.volume.as_str(),
                        node.as_str()
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            payload.map(|p| Ok(serde_json::from_str(&p)?)).transpose()
        })
        .await
    }
}

/// A MIME type the picker can filter on. Proton sometimes stores none.
pub fn picker_mime(asset: &Asset) -> String {
    if asset.mime.starts_with("image/") || asset.mime.starts_with("video/") {
        return asset.mime.clone();
    }
    let extension = asset
        .name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    let known = match extension.as_deref() {
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("webp") => Some("image/webp"),
        Some("heic") => Some("image/heic"),
        Some("heif") => Some("image/heif"),
        Some("avif") => Some("image/avif"),
        Some("gif") => Some("image/gif"),
        Some("tif" | "tiff") => Some("image/tiff"),
        Some("bmp") => Some("image/bmp"),
        Some("dng") => Some("image/x-adobe-dng"),
        Some("mp4" | "m4v") => Some("video/mp4"),
        Some("mov") => Some("video/quicktime"),
        Some("mkv") => Some("video/x-matroska"),
        Some("webm") => Some("video/webm"),
        Some("avi") => Some("video/x-msvideo"),
        Some("mts") => Some("video/mp2t"),
        _ => None,
    };
    known
        .unwrap_or(match asset.kind {
            MediaKind::Video => "video/mp4",
            _ => "image/jpeg",
        })
        .to_owned()
}
