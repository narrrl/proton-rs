//! Account-free encoded media fixtures, consumed through the regular media pipeline.
use crate::{App, Result};
use proton_gallery_rs::model::AssetKey;
use std::path::{Path, PathBuf};

#[async_trait::async_trait]
pub trait MediaProvider: Send + Sync {
    async fn thumbnail(&self, key: &AssetKey) -> Result<Option<Vec<u8>>>;
    async fn preview(&self, key: &AssetKey) -> Result<Option<Vec<u8>>>;
    async fn original_bytes(&self, _: &AssetKey, _: u64) -> Result<Vec<u8>> {
        Err(crate::Error::message("This fixture’s original is missing."))
    }
    async fn original(
        &self,
        key: &AssetKey,
        name: &str,
        directory: &Path,
        limit: u64,
    ) -> Result<PathBuf>;
}

#[async_trait::async_trait]
impl MediaProvider for App {
    async fn thumbnail(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        self.small_thumbnail(key).await
    }
    async fn preview(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        self.preview(key).await
    }
    async fn original_bytes(&self, key: &AssetKey, limit: u64) -> Result<Vec<u8>> {
        self.original_in_memory(key, limit).await
    }
    async fn original(
        &self,
        key: &AssetKey,
        name: &str,
        directory: &Path,
        limit: u64,
    ) -> Result<PathBuf> {
        self.download_original_limited(key, name, directory, limit)
            .await
    }
}

pub struct FixtureMedia;

impl FixtureMedia {
    fn bytes(key: &AssetKey) -> Result<Option<Vec<u8>>> {
        if key.account.as_str() != "synthetic" || key.volume.as_str() != "demo-volume" {
            return Err(crate::Error::message("Unknown fixture"));
        }
        let index: usize = key
            .node
            .as_str()
            .strip_prefix("asset-")
            .unwrap_or("")
            .parse()
            .map_err(|_| crate::Error::message("Unknown fixture"))?;
        Ok(match index % 17 {
            1 => None,
            2 => Some(b"unsupported image fixture".to_vec()),
            3 => {
                return Err(crate::Error::message(
                    "Fixture download failed. Retry with Refresh.",
                ));
            }
            _ => Some(
                match index % 5 {
                    0 => include_bytes!("../fixtures/0.png").as_slice(),
                    1 => include_bytes!("../fixtures/1.png").as_slice(),
                    2 => include_bytes!("../fixtures/2.png").as_slice(),
                    3 => include_bytes!("../fixtures/3.png").as_slice(),
                    _ => include_bytes!("../fixtures/4.png").as_slice(),
                }
                .to_vec(),
            ),
        })
    }
}

#[async_trait::async_trait]
impl MediaProvider for FixtureMedia {
    async fn thumbnail(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        Self::bytes(key)
    }
    async fn preview(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        Self::bytes(key)
    }
    async fn original(&self, _: &AssetKey, _: &str, _: &Path, _: u64) -> Result<PathBuf> {
        Err(crate::Error::message("This fixture’s original is missing."))
    }
}

/// Small deterministic collections for exercising album navigation and favorites.
pub async fn seed_collections(app: &App) -> Result<()> {
    use proton_gallery_rs::model::{AccountId, NodeId, SourceId, SourceSpec, VolumeId};
    let album_id = SourceId::new("demo-album")?;
    let account = AccountId::new("synthetic")?;
    let fresh = app.catalog.photo_albums(account.clone()).await?.is_empty();
    let page = app
        .catalog
        .window(
            Some(SourceId::new("demo")?),
            None,
            false,
            proton_gallery_rs::model::PageSize::new(12)?,
        )
        .await?;
    let mut assets = page.items;
    if let Some(asset) = assets.first_mut().filter(|_| fresh) {
        asset.favorite = true;
    }
    app.catalog
        .sync_photo_album(
            SourceSpec {
                id: album_id,
                account,
                volume: VolumeId::new("demo-volume")?,
                root: NodeId::new("demo-album")?,
                label: "Weekend photos".into(),
            },
            assets,
        )
        .await?;
    Ok(())
}
