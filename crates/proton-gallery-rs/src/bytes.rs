//! Revision-pinned range reads and authenticated, atomically published local blocks.
//!
//! Range mapping follows the existing `proton-stream` design: exact plaintext
//! sizes and prefix offsets, never division by a presumed Proton block size.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use zeroize::Zeroize;

use crate::catalog::Catalog;
use crate::model::{AssetKey, RevisionId};
use crate::source::BlockSource;
use crate::{CancellationToken, Error, Result};

pub const MAX_BLOCK_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BLOCKS: usize = 262_144;
const HEADER: &[u8; 4] = b"PGB1";
const OVERHEAD: u64 = 4 + 12 + 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DescriptorData {
    asset: AssetKey,
    revision: RevisionId,
    block_sizes: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "DescriptorData", into = "DescriptorData")]
pub struct RevisionDescriptor {
    data: DescriptorData,
    offsets: Vec<u64>,
    length: u64,
}

impl RevisionDescriptor {
    pub fn new(asset: AssetKey, revision: RevisionId, block_sizes: Vec<u64>) -> Result<Self> {
        DescriptorData {
            asset,
            revision,
            block_sizes,
        }
        .try_into()
    }

    pub fn asset(&self) -> &AssetKey {
        &self.data.asset
    }
    pub fn revision(&self) -> &RevisionId {
        &self.data.revision
    }
    pub fn block_sizes(&self) -> &[u64] {
        &self.data.block_sizes
    }
    pub fn length(&self) -> u64 {
        self.length
    }
    pub fn offset_of(&self, index: usize) -> Option<u64> {
        self.offsets.get(index).copied()
    }

    fn intersecting(&self, offset: u64, end: u64) -> std::ops::Range<usize> {
        if offset >= end || offset >= self.length {
            return 0..0;
        }
        let first = self.offsets.partition_point(|&start| start <= offset) - 1;
        let last = self
            .offsets
            .partition_point(|&start| start < end.min(self.length));
        first..last
    }

    fn identity(&self, index: usize) -> Result<Vec<u8>> {
        let length = self
            .block_sizes()
            .get(index)
            .ok_or_else(|| Error::Invalid("block index out of bounds".into()))?;
        Ok(serde_json::to_vec(&(
            1u32,
            self.asset(),
            self.revision(),
            index,
            length,
        ))?)
    }
}

impl TryFrom<DescriptorData> for RevisionDescriptor {
    type Error = Error;

    fn try_from(data: DescriptorData) -> Result<Self> {
        if data.block_sizes.len() > MAX_BLOCKS {
            return Err(Error::Invalid("too many blocks in revision".into()));
        }
        let mut offsets = Vec::with_capacity(data.block_sizes.len());
        let mut length = 0u64;
        for &size in &data.block_sizes {
            if size == 0 || size > MAX_BLOCK_BYTES {
                return Err(Error::Invalid(
                    "revision block exceeds supported size bounds".into(),
                ));
            }
            offsets.push(length);
            length = length
                .checked_add(size)
                .ok_or_else(|| Error::Invalid("revision size overflow".into()))?;
        }
        Ok(Self {
            data,
            offsets,
            length,
        })
    }
}

impl From<RevisionDescriptor> for DescriptorData {
    fn from(value: RevisionDescriptor) -> Self {
        value.data
    }
}

type BlockLock = Weak<AsyncMutex<()>>;

/// Callers supply a per-account key; key-store integration belongs to the host.
/// No key is persisted by this store, and names never become filesystem paths.
pub struct ObjectStore {
    root: PathBuf,
    cipher: Arc<Aes256Gcm>,
    flights: Mutex<HashMap<String, BlockLock>>,
    io_slots: Arc<Semaphore>,
    remote_slots: Semaphore,
}

impl ObjectStore {
    pub async fn open(root: impl AsRef<Path>, mut key: [u8; 32]) -> Result<Arc<Self>> {
        let cipher = Arc::new(Aes256Gcm::new((&key).into()));
        key.zeroize();
        let root = root.as_ref().to_owned();
        let create = root.clone();
        let check_cipher = cipher.clone();
        tokio::task::spawn_blocking(move || {
            private_directory(&create)?;
            check_store_key(&create, &check_cipher)
        })
        .await??;
        Ok(Arc::new(Self {
            root,
            cipher,
            flights: Mutex::new(HashMap::new()),
            io_slots: Arc::new(Semaphore::new(2)),
            remote_slots: Semaphore::new(2),
        }))
    }

    fn location(&self, identity: &[u8]) -> PathBuf {
        let digest = hex::encode(Sha256::digest(identity));
        self.root.join(&digest[..2]).join(&digest[2..])
    }

    fn lock_for(&self, identity: &[u8]) -> Arc<AsyncMutex<()>> {
        let key = hex::encode(Sha256::digest(identity));
        let mut flights = self
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lock) = flights.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        if flights.len() >= 1024 {
            flights.retain(|_, weak| weak.strong_count() != 0);
        }
        let lock = Arc::new(AsyncMutex::new(()));
        flights.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn load(&self, descriptor: &RevisionDescriptor, index: usize) -> Result<Option<Vec<u8>>> {
        let identity = descriptor.identity(index)?;
        let path = self.location(&identity);
        let expected = descriptor.block_sizes()[index];
        let cipher = self.cipher.clone();
        let permit = self
            .io_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Cancelled)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let file = match std::fs::File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if file.metadata()?.len() != expected + OVERHEAD {
                return Err(Error::CorruptObject);
            }
            let mut bytes = Vec::with_capacity((expected + OVERHEAD) as usize);
            file.take(expected + OVERHEAD + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 != expected + OVERHEAD || bytes.get(..4) != Some(HEADER) {
                return Err(Error::CorruptObject);
            }
            let nonce: [u8; 12] = bytes[4..16].try_into().map_err(|_| Error::CorruptObject)?;
            let plaintext = cipher
                .decrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: &bytes[16..],
                        aad: &identity,
                    },
                )
                .map_err(|_| Error::CorruptObject)?;
            if plaintext.len() as u64 != expected {
                return Err(Error::CorruptObject);
            }
            Ok(Some(plaintext))
        })
        .await?
    }

    async fn publish(
        &self,
        descriptor: &RevisionDescriptor,
        index: usize,
        plaintext: Arc<Vec<u8>>,
    ) -> Result<()> {
        if plaintext.len() as u64 != descriptor.block_sizes()[index] {
            return Err(Error::RevisionMismatch);
        }
        let identity = descriptor.identity(index)?;
        let path = self.location(&identity);
        let cipher = self.cipher.clone();
        let permit = self
            .io_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Cancelled)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let parent = path
                .parent()
                .ok_or_else(|| Error::Invalid("object path has no parent".into()))?;
            private_directory(parent)?;
            let mut nonce = [0u8; 12];
            getrandom::fill(&mut nonce).map_err(|_| Error::Random)?;
            let ciphertext = cipher
                .encrypt(
                    &Nonce::from(nonce),
                    Payload {
                        msg: plaintext.as_slice(),
                        aad: &identity,
                    },
                )
                .map_err(|_| Error::CorruptObject)?;
            let mut stage = tempfile::NamedTempFile::new_in(parent)?;
            stage.write_all(HEADER)?;
            stage.write_all(&nonce)?;
            stage.write_all(&ciphertext)?;
            stage.as_file().sync_all()?;
            // A failed prior object may be atomically replaced with validated bytes.
            stage.persist(&path).map_err(|e| Error::Io(e.error))?;
            #[cfg(unix)]
            {
                std::fs::File::open(parent)?.sync_all()?;
                if let Some(root) = parent.parent() {
                    std::fs::File::open(root)?.sync_all()?;
                }
            }
            Ok(())
        })
        .await?
    }
}

fn private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}

/// Fail closed if a host accidentally supplies a new key for retained data.
/// The marker contains only an authenticated empty payload, never the key.
fn check_store_key(root: &Path, cipher: &Aes256Gcm) -> Result<()> {
    const AAD: &[u8] = b"proton-gallery-store-key-v1";
    let path = root.join(".key-check");
    let validate = || -> Result<()> {
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(33)
            .read_to_end(&mut bytes)?;
        if bytes.len() != 32 || bytes.get(..4) != Some(b"PGK1") {
            return Err(Error::CorruptObject);
        }
        let nonce: [u8; 12] = bytes[4..16].try_into().map_err(|_| Error::CorruptObject)?;
        let message = cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &bytes[16..],
                    aad: AAD,
                },
            )
            .map_err(|_| Error::CorruptObject)?;
        if !message.is_empty() {
            return Err(Error::CorruptObject);
        }
        Ok(())
    };
    match validate() {
        Ok(()) => return Ok(()),
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // An existing nonempty store without its key marker needs recovery, not a
    // newly generated key that could overwrite the only retained originals.
    if std::fs::read_dir(root)?.next().transpose()?.is_some() {
        return Err(Error::CorruptObject);
    }
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).map_err(|_| Error::Random)?;
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: &[], aad: AAD })
        .map_err(|_| Error::CorruptObject)?;
    let mut stage = tempfile::NamedTempFile::new_in(root)?;
    stage.write_all(b"PGK1")?;
    stage.write_all(&nonce)?;
    stage.write_all(&ciphertext)?;
    stage.as_file().sync_all()?;
    match stage.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(Error::Io(error.error)),
    }
    #[cfg(unix)]
    {
        std::fs::File::open(root)?.sync_all()?;
        if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::File::open(parent)?.sync_all()?;
        }
    }
    validate()
}

#[derive(Clone)]
pub struct RangeReader {
    descriptor: RevisionDescriptor,
    store: Arc<ObjectStore>,
    remote: Option<Arc<dyn BlockSource>>,
}

impl RangeReader {
    pub fn new(
        descriptor: RevisionDescriptor,
        store: Arc<ObjectStore>,
        remote: Option<Arc<dyn BlockSource>>,
    ) -> Result<Self> {
        if remote
            .as_ref()
            .is_some_and(|remote| remote.descriptor() != &descriptor)
        {
            return Err(Error::RevisionMismatch);
        }
        Ok(Self {
            descriptor,
            store,
            remote,
        })
    }

    pub fn descriptor(&self) -> &RevisionDescriptor {
        &self.descriptor
    }

    async fn block(
        &self,
        index: usize,
        durable: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        let identity = self.descriptor.identity(index)?;
        let lock = self.store.lock_for(&identity);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = async {
                let _guard = lock.lock().await;
                match self.store.load(&self.descriptor, index).await {
                    Ok(Some(bytes)) => return Ok(bytes),
                    Ok(None) => {},
                    Err(Error::CorruptObject) if self.remote.is_some() => {},
                    Err(Error::Io(_)) if self.remote.is_some() => {},
                    Err(e) => return Err(e),
                }
                let source = self.remote.as_ref().ok_or(Error::OfflineMiss)?;
                let _permit = self.store.remote_slots.acquire().await.map_err(|_| Error::Cancelled)?;
                let bytes = source.read_block(index, cancel).await?;
                if bytes.len() as u64 != self.descriptor.block_sizes()[index] { return Err(Error::RevisionMismatch); }
                if cancel.is_cancelled() { return Err(Error::Cancelled); }
                let bytes = Arc::new(bytes);
                match self.store.publish(&self.descriptor, index, bytes.clone()).await {
                    Ok(()) => {},
                    // A disposable-cache write must not prevent online viewing.
                    // Retention always requires successful durable publication.
                    Err(Error::Io(_)) if !durable => {},
                    Err(error) => return Err(error),
                }
                Ok(Arc::try_unwrap(bytes).unwrap_or_else(|bytes| (*bytes).clone()))
            } => result,
        }
    }

    pub async fn read_at(
        &self,
        offset: u64,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if length > MAX_READ_BYTES {
            return Err(Error::Invalid("range read exceeds buffer limit".into()));
        }
        if offset >= self.descriptor.length() || length == 0 {
            return Ok(Vec::new());
        }
        let end = offset.saturating_add(length).min(self.descriptor.length());
        let mut output = Vec::with_capacity((end - offset) as usize);
        for index in self.descriptor.intersecting(offset, end) {
            let bytes = self.block(index, false, cancel).await?;
            let start = self.descriptor.offsets[index];
            let from = offset.saturating_sub(start) as usize;
            let to = (end - start).min(bytes.len() as u64) as usize;
            output.extend_from_slice(&bytes[from..to]);
        }
        Ok(output)
    }

    /// Checkpointed at whole-object boundaries. Restart reuses published blocks.
    /// Completion is local integrity/availability, not a remote signature claim.
    pub async fn keep_offline(
        &self,
        catalog: &Catalog,
        cancel: &CancellationToken,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        catalog.remember_revision(self.descriptor.clone()).await?;
        catalog
            .set_retained_complete(self.descriptor.clone(), false)
            .await?;
        let mut completed = 0;
        for index in 0..self.descriptor.block_sizes().len() {
            let bytes = self.block(index, true, cancel).await?;
            completed += bytes.len() as u64;
            progress(completed, self.descriptor.length());
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        catalog
            .set_retained_complete(self.descriptor.clone(), true)
            .await?;
        Ok(())
    }
}
