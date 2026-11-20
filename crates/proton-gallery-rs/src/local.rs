//! Managed device imports, separate from the encrypted Proton object store.
//! Hosts copy picker streams into `incoming`; this service owns publication.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::catalog::Catalog;
use crate::model::*;
use crate::{Error, Result};

#[derive(Clone)]
pub struct LocalLibrary {
    root: PathBuf,
    catalog: Catalog,
}

impl LocalLibrary {
    pub async fn open(root: impl AsRef<Path>, catalog: Catalog) -> Result<Self> {
        let root = root.as_ref().to_owned();
        let root = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
            std::fs::create_dir_all(root.join("incoming"))?;
            std::fs::create_dir_all(root.join("originals"))?;
            let root = root.canonicalize()?;
            #[cfg(unix)]
            {
                // Persist the new originals-directory entry before any import
                // can commit a catalog reference into it.
                File::open(&root)?.sync_all()?;
                if let Some(parent) = root.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Ok(root)
        })
        .await??;
        Ok(Self { root, catalog })
    }

    pub fn source() -> Result<SourceSpec> {
        Ok(SourceSpec {
            id: SourceId::new("local-device")?,
            account: AccountId::new("local-device")?,
            volume: VolumeId::new("local-device")?,
            root: NodeId::new("imports")?,
            label: "On this device".into(),
        })
    }

    pub fn incoming(&self) -> PathBuf {
        self.root.join("incoming")
    }

    /// Only accepts a regular file directly within the host's private staging folder.
    /// Hashing/copying is streaming (64 KiB), off the async executor. Publication
    /// and fsync precede the catalog commit. Interrupted imports leave no visible row.
    pub async fn import_staged(
        &self,
        staged: PathBuf,
        name: String,
        mime: String,
        time: i64,
    ) -> Result<AssetKey> {
        if name.is_empty() || name.len() > 16_384 || name.contains('\0') || mime.len() > 1024 {
            return Err(Error::Invalid("invalid import metadata".into()));
        }
        let kind = MediaKind::classify(&mime, &name)
            .ok_or_else(|| Error::Invalid("choose a photo or video".into()))?;
        let root = self.root.clone();
        let (hash, bytes, staged) =
            tokio::task::spawn_blocking(move || publish(&root, &staged)).await??;
        let source = Self::source()?;
        let key = AssetKey {
            account: source.account.clone(),
            volume: source.volume.clone(),
            node: NodeId::new(hash.clone())?,
        };
        let asset = Asset {
            key: key.clone(),
            name,
            mime,
            kind,
            sort_time: time,
            favorite: false,
            size_bytes: Some(bytes),
            revision: None,
        };
        self.catalog.commit_local(source, asset, hash).await?;
        // Cleanup is best effort after the durable commit. Never report a failed
        // import merely because a leftover staging file could not be removed.
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(staged)).await;
        Ok(key)
    }

    pub async fn file_for(&self, key: AssetKey) -> Result<Option<PathBuf>> {
        Ok(self.files_for(vec![key]).await?.pop().flatten())
    }

    pub async fn files_for(&self, keys: Vec<AssetKey>) -> Result<Vec<Option<PathBuf>>> {
        self.catalog
            .local_hashes(keys)
            .await?
            .into_iter()
            .map(|hash| hash.map(|hash| self.path_for_hash(&hash)).transpose())
            .collect()
    }

    fn path_for_hash(&self, hash: &str) -> Result<PathBuf> {
        if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Invalid("invalid stored content identity".into()));
        }
        Ok(self.root.join("originals").join(hash))
    }
}

fn publish(root: &Path, staged: &Path) -> Result<(String, u64, PathBuf)> {
    let metadata = std::fs::symlink_metadata(staged)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid("import needs a regular staging file".into()));
    }
    let staged = staged.canonicalize()?;
    if staged.parent() != Some(root.join("incoming").as_path()) {
        return Err(Error::Invalid(
            "import is outside the staging folder".into(),
        ));
    }
    let mut source = File::open(&staged)?;
    let originals = root.join("originals");
    let mut temp = tempfile::NamedTempFile::new_in(&originals)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65_536];
    let mut bytes = 0_u64;
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        temp.write_all(&buffer[..read])?;
        bytes = bytes
            .checked_add(read as u64)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| Error::Invalid("media is too large".into()))?;
    }
    if bytes == 0 {
        return Err(Error::Invalid("media is empty".into()));
    }
    let hash = hex::encode(hasher.finalize());
    temp.as_file().sync_all()?;
    // Replacing identical content also repairs a truncated local original.
    temp.persist(originals.join(&hash))
        .map_err(|e| Error::Io(e.error))?;
    #[cfg(unix)]
    File::open(&originals)?.sync_all()?;
    Ok((hash, bytes, staged))
}
