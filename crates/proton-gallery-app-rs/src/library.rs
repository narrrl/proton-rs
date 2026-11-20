//! What a view shows: its outline for the grid, and the records of the items
//! the grid makes visible.
//!
//! The grid plans every row from the outline (identity, time and shape of
//! each item) and asks for the records of the rows it shows. The layout
//! therefore stays put while records load, and only a new outline, made
//! after media was added or removed, moves it.
use super::*;
use proton_gallery_rs::model::{
    AccountId, AssetKey, MediaKind, PageSize, SourceId, TimelineCursor,
};
use proton_gallery_rs::outline::OutlineEntry;

/// A place in the library.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// The account's Proton Photos timeline, merged with the phone's media.
    Photos,
    /// Media saved in the app's own library.
    Device,
    /// An album belonging to the signed-in Proton Photos account.
    PhotoAlbum(String),
    /// One album of a Drive folder gallery.
    DriveAlbum { gallery: String, album: String },
    /// Any other catalog source, such as the synthetic demo library.
    Source(String),
}

/// One photo or video, as a frontend shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaItem {
    /// Stable within the catalog; see [`item_id`].
    pub id: String,
    pub key: AssetKey,
    pub name: String,
    pub mime: String,
    pub is_video: bool,
    pub size_bytes: u64,
    pub sort_time: i64,
    /// A file on this device holding the item, when there is one.
    pub local_path: Option<PathBuf>,
    pub aspect_ratio: Option<f64>,
    /// Fetched from Proton: no local file and not phone media.
    pub remote: bool,
    pub revision: Option<String>,
    /// The phone's media identifier when the file is on the phone.
    pub phone_id: Option<String>,
    /// Whether Proton holds a copy; false only for phone media.
    pub backed_up: bool,
    /// A Proton photo or Drive file, which can be moved to the Proton trash.
    pub in_proton: bool,
    pub favorite: bool,
}

/// Every item of a view in display order. Taken once; later changes to the
/// catalog need a new outline.
#[derive(Debug, Default)]
pub struct Outline {
    pub keys: Vec<AssetKey>,
    pub ids: Vec<String>,
    pub times: Vec<i64>,
    /// Width divided by height, or 0 when not known yet.
    pub ratios: Vec<f32>,
}

impl Outline {
    fn new(entries: Vec<OutlineEntry>) -> Result<Self> {
        let mut outline = Self {
            keys: Vec::with_capacity(entries.len()),
            ids: Vec::with_capacity(entries.len()),
            times: Vec::with_capacity(entries.len()),
            ratios: Vec::with_capacity(entries.len()),
        };
        for entry in entries {
            outline.ids.push(item_id(&entry.key)?);
            outline.times.push(entry.sort_time);
            outline.ratios.push(if entry.height > 0 {
                entry.width as f32 / entry.height as f32
            } else {
                0.0
            });
            outline.keys.push(entry.key);
        }
        Ok(outline)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A page of a view, with cursors to the pages around it.
#[derive(Clone, Debug)]
pub struct Page {
    pub items: Vec<MediaItem>,
    pub next: Option<String>,
    pub previous: Option<String>,
}

/// The id of an item: account, volume and node, so different sources cannot
/// collide. Media saved in the app keeps its plain node id.
pub fn item_id(key: &AssetKey) -> Result<String> {
    Ok(if key.account.as_str() == "local-device" {
        key.node.to_string()
    } else {
        key.stable_id()?
    })
}

impl App {
    /// The account's Photos source; `None` before Photos is first synced.
    pub async fn photo_source(&self) -> Result<Option<SourceId>> {
        let Some(account) = self.auth.account_id() else {
            return Ok(None);
        };
        if let Some((source, _)) = self.catalog.account_photos_sync(account.clone()).await? {
            return Ok(Some(source));
        }
        Ok(self
            .catalog
            .account_sources(SourceKind::Photos, account)
            .await?
            .first()
            .map(|s| s.id.clone()))
    }

    /// The catalog source behind a view; `None` before Photos is first synced.
    pub async fn view_source(&self, view: View) -> Result<Option<SourceId>> {
        Ok(match view {
            View::Device => Some(LocalLibrary::source()?.id),
            View::Photos => self.photo_source().await?,
            View::PhotoAlbum(id) => {
                let account = self
                    .auth
                    .account_id()
                    .ok_or(proton_gallery_rs::Error::NotFound)?;
                let id = SourceId::new(id)?;
                self.catalog
                    .photo_album(account, id.clone())
                    .await?
                    .ok_or(proton_gallery_rs::Error::NotFound)?;
                if self.catalog.album_settings(id.clone()).await?.hidden {
                    return Err(proton_gallery_rs::Error::NotFound.into());
                }
                Some(id)
            }
            View::DriveAlbum { gallery, album } => Some(self.checked_album(gallery, album).await?),
            View::Source(id) => Some(SourceId::new(id)?),
        })
    }

    /// An album of the signed-in account's galleries.
    pub async fn checked_album(&self, gallery: String, album: String) -> Result<SourceId> {
        let account = self
            .auth
            .account_id()
            .ok_or(proton_gallery_rs::Error::NotFound)?;
        let gallery = SourceId::new(gallery)?;
        let album = SourceId::new(album)?;
        if !self
            .catalog
            .source_belongs_to(album.clone(), account)
            .await?
            || !self.catalog.album_exists(gallery, album.clone()).await?
        {
            return Err(proton_gallery_rs::Error::NotFound.into());
        }
        Ok(album)
    }

    /// The outline of `view`.
    pub async fn outline(&self, view: View) -> Result<Outline> {
        self.outline_filtered(view, false).await
    }

    /// Filter compact identities in SQLite, without reading every media record.
    pub async fn outline_filtered(&self, view: View, favorites: bool) -> Result<Outline> {
        self.outline_with_visibility(view, favorites, false).await
    }

    pub async fn outline_with_visibility(
        &self,
        view: View,
        favorites: bool,
        unlocked: bool,
    ) -> Result<Outline> {
        let remote = matches!(
            view,
            View::Photos | View::PhotoAlbum(_) | View::DriveAlbum { .. }
        );
        if remote {
            self.require_gallery_visibility().await?;
        }
        let entries = if matches!(view, View::Photos) {
            if favorites {
                self.catalog
                    .timeline_favorites_outline(self.photo_source().await?, self.auth.account_id())
                    .await?
            } else {
                self.catalog
                    .timeline_outline(self.photo_source().await?, self.auth.account_id())
                    .await?
            }
        } else {
            let source = if let View::PhotoAlbum(id) = &view
                && unlocked
            {
                let account = self
                    .auth
                    .account_id()
                    .ok_or(proton_gallery_rs::Error::NotFound)?;
                let id = SourceId::new(id)?;
                Some(
                    self.catalog
                        .photo_album(account, id.clone())
                        .await?
                        .ok_or(proton_gallery_rs::Error::NotFound)?
                        .id,
                )
            } else if let View::DriveAlbum { gallery, album } = &view {
                let id = self.checked_album(gallery.clone(), album.clone()).await?;
                if !unlocked
                    && self
                        .catalog
                        .drive_album_is_private(SourceId::new(gallery)?, id.clone())
                        .await?
                {
                    return Err(proton_gallery_rs::Error::NotFound.into());
                }
                Some(id)
            } else {
                self.view_source(view).await?
            };
            match source {
                Some(source) => {
                    let order = self.catalog.album_settings(source.clone()).await?.order;
                    if remote && !unlocked {
                        self.catalog
                            .visible_source_outline(source, order, favorites)
                            .await?
                    } else if favorites {
                        self.catalog.source_favorites_outline(source, order).await?
                    } else {
                        self.catalog.source_outline(source, order).await?
                    }
                }
                None => vec![],
            }
        };
        Outline::new(entries)
    }

    /// The records of up to `count` items from `start` of `outline`. An item
    /// removed since the outline was taken is `None`.
    pub async fn outline_items(
        &self,
        outline: &Outline,
        start: usize,
        count: usize,
    ) -> Result<Vec<Option<MediaItem>>> {
        let start = start.min(outline.len());
        let end = start
            .saturating_add(count.min(PageSize::MAX))
            .min(outline.len());
        let found = self
            .catalog
            .assets_by_key(outline.keys[start..end].to_vec())
            .await?;
        let present: Vec<usize> = found
            .iter()
            .enumerate()
            .filter_map(|(index, asset)| asset.as_ref().map(|_| index))
            .collect();
        let mut items = self
            .media_items(found.iter().flatten().cloned().collect())
            .await?
            .into_iter();
        let mut result = vec![None; found.len()];
        for index in present {
            result[index] = items.next();
        }
        Ok(result)
    }

    /// A page of `view` after (or, `backwards`, before) `cursor`.
    pub async fn page(
        &self,
        view: View,
        cursor: Option<&str>,
        backwards: bool,
        limit: usize,
    ) -> Result<Page> {
        let cursor = cursor.map(TimelineCursor::decode).transpose()?;
        let size = PageSize::new(limit)?;
        let page = if matches!(view, View::Photos) {
            self.catalog
                .timeline_window(
                    self.photo_source().await?,
                    self.auth.account_id(),
                    cursor,
                    backwards,
                    size,
                )
                .await?
        } else {
            let Some(source) = self.view_source(view).await? else {
                return Ok(Page {
                    items: vec![],
                    next: None,
                    previous: None,
                });
            };
            let order = self.catalog.album_settings(source.clone()).await?.order;
            self.catalog
                .window_ordered(Some(source), cursor, backwards, order, size)
                .await?
        };
        Ok(Page {
            items: self.media_items(page.items).await?,
            next: page.next.map(|c| c.encode()).transpose()?,
            previous: page.previous.map(|c| c.encode()).transpose()?,
        })
    }

    /// How many items `view` holds.
    pub async fn count(&self, view: View) -> Result<u64> {
        if matches!(view, View::Photos) {
            let photos = self.photo_source().await?;
            return Ok(self
                .catalog
                .timeline_count(photos, self.auth.account_id())
                .await?);
        }
        match self.view_source(view).await? {
            Some(source) => Ok(self.catalog.source_count(source).await?),
            None => Ok(0),
        }
    }

    /// Frontend records for catalog assets, with local files and known shapes.
    pub async fn media_items(
        &self,
        assets: Vec<proton_gallery_rs::model::Asset>,
    ) -> Result<Vec<MediaItem>> {
        let keys: Vec<AssetKey> = assets.iter().map(|asset| asset.key.clone()).collect();
        let paths = self.local.files_for(keys.clone()).await?;
        let ratios = self.catalog.aspect_ratios(keys.clone()).await?;
        let links = self
            .catalog
            .phone_links(self.auth.account_id(), keys)
            .await?;
        let mut items = Vec::with_capacity(assets.len());
        for (((asset, path), ratio), link) in assets.into_iter().zip(paths).zip(ratios).zip(links) {
            let phone = proton_gallery_rs::phone::is_phone(&asset.key);
            items.push(MediaItem {
                id: item_id(&asset.key)?,
                remote: path.is_none() && !phone,
                in_proton: !phone && asset.key.account.as_str() != "local-device",
                key: asset.key,
                revision: asset.revision.map(|revision| revision.to_string()),
                name: asset.name,
                mime: asset.mime,
                is_video: asset.kind == MediaKind::Video,
                size_bytes: asset.size_bytes.unwrap_or(0),
                sort_time: asset.sort_time,
                local_path: path,
                aspect_ratio: ratio,
                phone_id: link.phone,
                backed_up: link.backed_up,
                favorite: asset.favorite,
            });
        }
        Ok(items)
    }

    /// Whether `key` belongs to the signed-in account.
    pub(crate) fn owns(&self, account: &AccountId, key: &AssetKey) -> Result<()> {
        if &key.account == account {
            Ok(())
        } else {
            Err(proton_gallery_rs::Error::NotFound.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use proton_gallery_rs::CancellationToken;
    use proton_gallery_rs::synthetic::SyntheticSource;

    #[tokio::test]
    async fn outline_items_follow_the_outline_and_stay_bounded() {
        let (_dir, app) = app(Arc::default()).await;
        let source = SyntheticSource::new(1_000).unwrap();
        app.index_source(&source, app.generation(), &CancellationToken::new(), |_| {})
            .await
            .unwrap();
        let view = View::Source("demo".into());
        let outline = app.outline(view.clone()).await.unwrap();
        assert_eq!(outline.len(), 1_000);
        assert_eq!(app.count(view).await.unwrap(), 1_000);
        let items = app.outline_items(&outline, 990, 50).await.unwrap();
        assert_eq!(items.len(), 10);
        let first = items[0].as_ref().unwrap();
        assert_eq!(first.id, outline.ids[990]);
        assert!(first.remote && first.in_proton);
        // A request past the end is empty, and requests are capped.
        assert!(
            app.outline_items(&outline, 5_000, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(app.outline_items(&outline, 0, 100_000).await.unwrap().len() <= PageSize::MAX);
    }

    #[tokio::test]
    async fn favorite_outlines_follow_catalog_changes_and_exclude_hidden_photos() {
        let (_dir, app) = app(Arc::default()).await;
        let source = SyntheticSource::new(20).unwrap();
        app.index_source(&source, app.generation(), &CancellationToken::new(), |_| {})
            .await
            .unwrap();
        crate::fixtures::seed_collections(&app).await.unwrap();
        let demo = View::Source("demo".into());
        let album = SourceId::new("demo-album").unwrap();
        let outline = app.outline_filtered(demo.clone(), true).await.unwrap();
        assert_eq!(outline.len(), 1);
        assert_eq!(
            app.outline(View::Source(album.to_string()))
                .await
                .unwrap()
                .len(),
            12
        );
        let account = AccountId::new("synthetic").unwrap();
        let photos = SourceId::new("demo").unwrap();
        assert_eq!(
            app.catalog
                .timeline_favorites_outline(Some(photos.clone()), Some(account.clone()))
                .await
                .unwrap()[0]
                .key,
            outline.keys[0]
        );
        app.catalog
            .set_album_settings(
                album,
                proton_gallery_rs::library::AlbumSettings {
                    hidden: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            app.catalog
                .timeline_favorites_outline(Some(photos), Some(account))
                .await
                .unwrap()
                .is_empty()
        );
        app.catalog.set_favorite(outline.keys, false).await.unwrap();
        assert!(app.outline_filtered(demo, true).await.unwrap().is_empty());
    }
}
