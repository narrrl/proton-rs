//! Media bytes from Proton: small thumbnails, screen-size previews, and full
//! originals decrypted into a directory the frontend owns.
use super::*;
use proton_gallery_rs::model::AssetKey;
use std::io::Write;

/// Largest thumbnail accepted from Proton.
pub const THUMBNAIL_LIMIT: usize = 4 * 1024 * 1024;
/// Largest preview accepted from Proton.
pub const PREVIEW_LIMIT: usize = 16 * 1024 * 1024;

/// A file name that is safe on any file system and keeps the extension.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    let mut out: String = if trimmed.is_empty() {
        "photo".into()
    } else {
        trimmed.into()
    };
    while out.len() > 120 {
        // Keep the end, which holds the extension.
        let cut = out.char_indices().nth(1).map_or(out.len(), |(i, _)| i);
        out.drain(..cut);
    }
    out
}

/// A folder per item, so two photos with one name never collide.
fn item_folder(key: &AssetKey) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!(
        "{}/{}/{}",
        key.account.as_str(),
        key.volume.as_str(),
        key.node.as_str()
    ));
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

impl App {
    /// Decrypt a bounded original into memory for an unlocked hidden album.
    pub async fn original_in_memory(&self, key: &AssetKey, limit: u64) -> Result<Vec<u8>> {
        let _operations = self.operations.read().await;
        let account = self.auth.account().await?;
        self.owns(&account.id, key)?;
        self.require_gallery_visibility().await?;
        let photos_volume = self.photos_volume(&account.id).await?;
        let uid = proton_gallery_drive_rs::node_uid(key);
        let mut out = LimitedWriter {
            inner: Vec::new(),
            left: limit,
        };
        let download = async {
            if photos_volume.as_ref() == Some(&key.volume) {
                proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone())
                    .download_photo_to(&uid, &mut out)
                    .await
            } else {
                account.drive.download_file_to(&uid, &mut out).await
            }
        };
        tokio::select! { biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            result = download => result.map_err(|e| {
                tracing::warn!("hidden original download failed: {}", proton_gallery_drive_rs::auth::cause_chain(&e));
                Error::message("Couldn’t open the original within the memory limit.")
            })?,
        }
        Ok(out.inner)
    }

    /// The volume of the account's Photos, from the open source or the catalog.
    pub(crate) async fn photos_volume(
        &self,
        account: &proton_gallery_rs::model::AccountId,
    ) -> Result<Option<proton_gallery_rs::model::VolumeId>> {
        let cached = self
            .photos
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|source| {
                proton_gallery_rs::source::MediaSource::spec(source.as_ref()).account == *account
            })
            .map(|source| {
                proton_gallery_rs::source::MediaSource::spec(source.as_ref())
                    .volume
                    .clone()
            });
        if cached.is_some() {
            return Ok(cached);
        }
        Ok(self
            .catalog
            .account_sources(SourceKind::Photos, account.clone())
            .await?
            .first()
            .map(|s| s.volume.clone()))
    }

    /// A bounded thumbnail for the grid. A preview can supply it when Proton
    /// only generated the larger rendition.
    pub async fn thumbnail(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        use proton_drive_rs::ThumbnailType;
        let fetch = async {
            match self
                .rendition(key, ThumbnailType::Thumbnail, THUMBNAIL_LIMIT)
                .await?
            {
                Some(bytes) => Ok(Some(bytes)),
                None => {
                    self.rendition(key, ThumbnailType::Preview, PREVIEW_LIMIT)
                        .await
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(12), fetch)
            .await
            .map_err(|_| Error::message("Proton took too long to load this thumbnail."))?
    }

    /// Only the small rendition, for callers that can try alternatives after decoding.
    pub async fn small_thumbnail(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        self.rendition(
            key,
            proton_drive_rs::ThumbnailType::Thumbnail,
            THUMBNAIL_LIMIT,
        )
        .await
    }

    /// The large preview (about screen size) Proton keeps beside a photo.
    pub async fn preview(&self, key: &AssetKey) -> Result<Option<Vec<u8>>> {
        self.rendition(key, proton_drive_rs::ThumbnailType::Preview, PREVIEW_LIMIT)
            .await
    }

    async fn rendition(
        &self,
        key: &AssetKey,
        kind: proton_drive_rs::ThumbnailType,
        limit: usize,
    ) -> Result<Option<Vec<u8>>> {
        let _operations = self.operations.read().await;
        let account = self.auth.account().await?;
        self.owns(&account.id, key)?;
        let photos_volume = self.photos_volume(&account.id).await?;
        let uid = proton_gallery_drive_rs::node_uid(key);
        let download = async {
            if photos_volume.as_ref() == Some(&key.volume) {
                proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone())
                    .download_thumbnail(&uid, kind)
                    .await
            } else {
                account.drive.download_thumbnail(&uid, kind).await
            }
        };
        let bytes = tokio::select! { biased;
            _ = account.cancel.cancelled() => return Err(Error::Cancelled),
            result = download => result.map_err(|e| {
                tracing::warn!("Proton rendition failed: {}", proton_gallery_drive_rs::auth::cause_chain(&e));
                Error::message("Couldn’t load this photo.")
            })?,
        };
        if bytes.as_ref().is_some_and(|b| b.len() > limit) {
            return Err(proton_gallery_rs::Error::Invalid("preview exceeds budget".into()).into());
        }
        Ok(bytes)
    }

    /// Decrypt a Proton photo's or video's original into `directory` and
    /// return its path. A finished earlier download is reused.
    pub async fn download_original(
        &self,
        key: &AssetKey,
        name: &str,
        directory: &Path,
    ) -> Result<PathBuf> {
        self.download_original_limited(key, name, directory, u64::MAX)
            .await
    }

    /// As above, with a byte limit enforced while streaming, before writing.
    pub async fn download_original_limited(
        &self,
        key: &AssetKey,
        name: &str,
        directory: &Path,
        limit: u64,
    ) -> Result<PathBuf> {
        let _operations = self.operations.read().await;
        let generation = self.generation();
        let account = self.auth.account().await?;
        self.owns(&account.id, key)?;
        let no_room = || Error::message("There isn’t room on this device to open the original.");
        let folder = directory.join(item_folder(key));
        let path = folder.join(safe_name(name));
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() > 0 && m.len() <= limit) {
            return Ok(path);
        }
        let photos_volume = self.photos_volume(&account.id).await?;
        std::fs::create_dir_all(&folder).map_err(|_| no_room())?;
        // Unique name and RAII deletion cover errors, dropped futures and rename failures.
        let part = staged_download(&folder).map_err(|_| no_room())?;
        let mut out = LimitedWriter {
            inner: std::io::BufWriter::new(part.as_file()),
            left: limit,
        };
        let uid = proton_gallery_drive_rs::node_uid(key);
        let download = async {
            if photos_volume.as_ref() == Some(&key.volume) {
                proton_drive_rs::ProtonPhotosClient::from_drive_client(account.drive.clone())
                    .download_photo_to(&uid, &mut out)
                    .await
            } else {
                account.drive.download_file_to(&uid, &mut out).await
            }
        };
        let result = tokio::select! { biased;
            _ = account.cancel.cancelled() => Err(Error::Cancelled),
            result = download => result.map_err(|e| {
                tracing::warn!("original download failed: {}", proton_gallery_drive_rs::auth::cause_chain(&e));
                Error::message("Couldn’t download the original. Check your connection and try again.")
            }),
        };
        let finished = result.and_then(|()| out.flush().map_err(|_| no_room()));
        drop(out);
        finished?;
        self.ensure_current(generation)?;
        if account.cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        part.persist(&path).map_err(|_| no_room())?;
        Ok(path)
    }

    /// Remember a photo's pixel size, learned from its decoded thumbnail, so
    /// the next outline lays it out at its real shape.
    pub async fn remember_shape(&self, key: AssetKey, width: u32, height: u32) -> Result<()> {
        let _operations = self.operations.read().await;
        let account = self
            .auth
            .account_id()
            .ok_or(proton_gallery_rs::Error::NotFound)?;
        self.owns(&account, &key)?;
        self.catalog.remember_shape(key, width, height).await?;
        Ok(())
    }
}

fn staged_download(folder: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    tempfile::Builder::new()
        .prefix(".download-")
        .tempfile_in(folder)
}

struct LimitedWriter<W> {
    inner: W,
    left: u64,
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.left {
            return Err(std::io::Error::other("Original exceeds viewing budget"));
        }
        let written = self.inner.write(bytes)?;
        self.left -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    #[test]
    fn names_are_safe_and_keep_their_extension() {
        assert_eq!(safe_name("IMG_1.jpg"), "IMG_1.jpg");
        assert_eq!(safe_name("../a/b:c.png"), "_a_b_c.png");
        assert_eq!(safe_name("..."), "photo");
        let long = format!("{}.heic", "x".repeat(300));
        let safe = safe_name(&long);
        assert!(safe.len() <= 120 && safe.ends_with(".heic"));
    }

    #[tokio::test]
    async fn media_of_another_account_is_refused() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (_dir, app) = app(vault).await;
        app.restore_session().await.unwrap();
        let key: AssetKey = serde_json::from_value(serde_json::json!({
            "account": "user-2", "volume": "volume", "node": "node"
        }))
        .unwrap();
        assert!(app.thumbnail(&key).await.is_err());
        assert!(app.remember_shape(key, 4, 3).await.is_err());
    }
    #[tokio::test]
    async fn a_cached_original_never_bypasses_account_authorization() {
        let vault = Arc::new(MemoryVault::default());
        *vault.saved.lock().unwrap() = Some(saved_session("user-1"));
        let (dir, app) = app(vault).await;
        app.restore_session().await.unwrap();
        let key: AssetKey = serde_json::from_value(serde_json::json!({
            "account": "user-2", "volume": "volume", "node": "node"
        }))
        .unwrap();
        let folder = dir.path().join(item_folder(&key));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("photo.jpg"), b"cached original").unwrap();
        assert!(
            app.download_original(&key, "photo.jpg", dir.path())
                .await
                .is_err()
        );
        app.sign_out().await.unwrap();
        assert!(
            app.download_original(&key, "photo.jpg", dir.path())
                .await
                .is_err()
        );
    }

    #[test]
    fn original_streaming_enforces_the_budget_before_writing_the_next_chunk() {
        let mut writer = LimitedWriter {
            inner: Vec::new(),
            left: 4,
        };
        writer.write_all(b"123").unwrap();
        assert!(writer.write_all(b"45").is_err());
        assert_eq!(writer.inner, b"123");
    }
    #[tokio::test]
    async fn interrupted_downloads_remove_partial_files_when_the_future_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let run = async {
            let mut part = staged_download(dir.path()).unwrap();
            part.write_all(b"incomplete plaintext").unwrap();
            std::future::pending::<()>().await;
            part.persist(dir.path().join("finished.jpg")).unwrap();
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), run)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_failed_publication_also_removes_the_partial_original() {
        let dir = tempfile::tempdir().unwrap();
        let part = staged_download(dir.path()).unwrap();
        assert!(part.persist(dir.path()).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
