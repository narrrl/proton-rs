//! Incremental location indexing of the visible Proton Photos timeline.
use crate::{App, Error, Generation, Outline, Result, View};
use futures_util::{StreamExt, stream};
use proton_gallery_rs::model::{Asset, MediaKind};
use proton_gallery_rs::places::{Coordinates, embedded_coordinates};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const SCAN_BATCH: usize = 24;
const EXIF_PREFIX: u64 = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
static EXIF_READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

#[derive(Debug)]
pub struct PlacesScan {
    pub outline: Arc<Outline>,
    generation: Generation,
    demo: bool,
}

#[derive(Debug, Clone)]
pub struct LocatedPhoto {
    pub index: usize,
    pub name: String,
    pub coordinates: Coordinates,
    pub place: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PlacesBatch {
    pub photos: Vec<LocatedPhoto>,
    pub next: usize,
    pub failed: usize,
}

impl App {
    pub async fn places_scan(&self) -> Result<PlacesScan> {
        let generation = self.generation();
        let account = self.auth.account().await?;
        let volume = self.photos_volume(&account.id).await?;
        let visible = self.outline(View::Photos).await?;
        let mut outline = Outline::default();
        for (index, key) in visible.keys.into_iter().enumerate() {
            if key.account == account.id && Some(&key.volume) == volume.as_ref() {
                outline.keys.push(key);
                outline.ids.push(visible.ids[index].clone());
                outline.ratios.push(visible.ratios[index]);
                outline.times.push(visible.times[index]);
            }
        }
        self.ensure_current(generation)?;
        Ok(PlacesScan {
            outline: Arc::new(outline),
            generation,
            demo: false,
        })
    }

    pub async fn demo_places(&self) -> Result<PlacesScan> {
        Ok(PlacesScan {
            outline: Arc::new(self.outline(View::Source("demo".into())).await?),
            generation: self.generation(),
            demo: true,
        })
    }

    /// Cached coordinates are available even when a request to Proton fails.
    /// A failed lookup stays pending so a retry can recover it.
    pub async fn places_batch(&self, scan: &PlacesScan, start: usize) -> Result<PlacesBatch> {
        self.ensure_current(scan.generation)?;
        if scan.demo {
            let photos = [
                ("Berlin", 52.52, 13.405),
                ("Berlin", 52.52, 13.405),
                ("Paris", 48.86, 2.35),
                ("Oslo", 59.91, 10.75),
                ("Reykjavík", 64.14, -21.94),
            ]
            .into_iter()
            .enumerate()
            .filter(|(index, _)| *index < scan.outline.len())
            .map(|(index, (name, latitude, longitude))| LocatedPhoto {
                index,
                name: format!("Photo {} in {name}", index + 1),
                coordinates: Coordinates {
                    latitude,
                    longitude,
                },
                place: Some(name.into()),
            })
            .collect();
            return Ok(PlacesBatch {
                next: scan.outline.len(),
                photos,
                failed: 0,
            });
        }
        let _operations = self.operations.read().await;
        self.require_gallery_visibility().await?;
        let account = self.auth.account().await?;
        let start = start.min(scan.outline.len());
        let next = (start + SCAN_BATCH).min(scan.outline.len());
        let keys = &scan.outline.keys[start..next];
        let visible = self.catalog.visible_location_keys(keys.to_vec()).await?;
        let assets = self.catalog.assets_by_key(keys.to_vec()).await?;
        let mut batch = PlacesBatch {
            next,
            ..Default::default()
        };
        let mut candidates = Vec::new();
        for (offset, asset) in assets.into_iter().enumerate() {
            if let Some(asset) = asset.filter(|asset| {
                asset.kind == MediaKind::Image
                    && asset.key.account == account.id
                    && visible.contains(&asset.key)
            }) {
                candidates.push((start + offset, asset));
            }
        }
        let cached = self
            .catalog
            .photo_locations(
                candidates
                    .iter()
                    .map(|(_, asset)| {
                        (
                            asset.key.clone(),
                            asset.revision.as_ref().map(ToString::to_string),
                        )
                    })
                    .collect(),
            )
            .await?;
        let photos = proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone());
        let unknown: Vec<_> = candidates
            .iter()
            .zip(&cached)
            .filter(|(_, status)| status.is_none())
            .map(|((_, asset), _)| proton_gallery_drive_rs::node_uid(&asset.key))
            .collect();
        let mut nodes = if unknown.is_empty() {
            HashMap::new()
        } else {
            tokio::select! {
                biased;
                _ = account.cancel.cancelled() => return Err(Error::Cancelled),
                response = tokio::time::timeout(REQUEST_TIMEOUT, photos.enumerate_nodes(&unknown)) => {
                    response.ok().and_then(std::result::Result::ok).unwrap_or_default()
                        .into_iter().filter(|node| !node.trashed)
                        .map(|node| (node.uid.clone(), node)).collect()
                }
            }
        };
        let work = candidates
            .into_iter()
            .zip(cached)
            .map(|((index, asset), cached)| {
                let node = nodes.remove(&proton_gallery_drive_rs::node_uid(&asset.key));
                let drive = &account.drive;
                async move {
                    let fresh = cached.is_none();
                    let location = if let Some(cached) = cached {
                        Ok(cached)
                    } else if let Some(node) = node {
                        if let Some(location) = node
                            .photo
                            .and_then(|metadata| metadata.location)
                            .and_then(|location| {
                                Coordinates::new(location.latitude, location.longitude)
                            })
                        {
                            Ok(Some(location))
                        } else {
                            read_embedded_location(drive, &asset).await
                        }
                    } else {
                        Err(())
                    };
                    (index, asset, fresh, location)
                }
            });
        let completed = tokio::select! {
            biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            completed = stream::iter(work).buffered(4).collect::<Vec<_>>() => completed,
        };
        let mut found = Vec::new();
        for (index, asset, fresh, location) in completed {
            let Ok(location) = location else {
                batch.failed += 1;
                continue;
            };
            if fresh {
                found.push((
                    asset.key,
                    asset.revision.as_ref().map(ToString::to_string),
                    location,
                ));
            }
            if let Some(coordinates) = location {
                batch.photos.push(LocatedPhoto {
                    index,
                    name: asset.name,
                    coordinates,
                    place: proton_gallery_rs::places::nearest(
                        coordinates.latitude,
                        coordinates.longitude,
                    )
                    .map(|city| city.name.to_owned()),
                });
            }
        }
        self.ensure_current(scan.generation)?;
        self.require_gallery_visibility().await?;
        if !found.is_empty() {
            self.catalog.store_photo_locations(found).await?;
        }
        let visible = self
            .catalog
            .visible_location_keys(
                batch
                    .photos
                    .iter()
                    .map(|photo| scan.outline.keys[photo.index].clone())
                    .collect(),
            )
            .await?;
        batch
            .photos
            .retain(|photo| visible.contains(&scan.outline.keys[photo.index]));
        Ok(batch)
    }
}

async fn read_embedded_location(
    drive: &proton_drive_rs::ProtonDriveClient,
    asset: &Asset,
) -> Result<Option<Coordinates>, ()> {
    let name = asset.name.to_ascii_lowercase();
    let supports_exif = matches!(
        name.rsplit('.').next().unwrap_or_default(),
        "jpg"
            | "jpeg"
            | "tif"
            | "tiff"
            | "heic"
            | "heif"
            | "png"
            | "webp"
            | "dng"
            | "arw"
            | "cr2"
            | "nef"
    ) || matches!(asset.mime.as_str(), "image/jpeg" | "image/tiff");
    if !supports_exif {
        return Ok(None);
    }
    let _permit = EXIF_READS.acquire().await.map_err(|_| ())?;
    let uid = proton_gallery_drive_rs::node_uid(&asset.key);
    let fetch = async {
        let reader = if let Some(revision) = &asset.revision {
            drive.open_revision_at(&uid, revision.as_str()).await?
        } else {
            drive.open_revision(&uid).await?
        };
        reader.read_at(0, reader.size().min(EXIF_PREFIX)).await
    };
    let bytes = tokio::time::timeout(REQUEST_TIMEOUT, fetch)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    Ok(embedded_coordinates(&bytes))
}
