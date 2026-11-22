//! A fully downloaded revision as a [`BlockSource`]: the offline copy plays
//! through the same seekable stream as the network one.

use std::path::PathBuf;

use async_trait::async_trait;

use super::source::BlockSource;
use crate::{BlockMap, Error, Result};

/// A fully downloaded revision, read block by block from one file.
pub struct FileBlocks {
    revision_id: String,
    path: PathBuf,
    sizes: Vec<u64>,
    map: BlockMap,
}
impl FileBlocks {
    pub fn new(revision_id: String, path: PathBuf, sizes: Vec<u64>) -> Self {
        let map = BlockMap::new(&sizes);
        Self {
            revision_id,
            path,
            sizes,
            map,
        }
    }
}
#[async_trait]
impl BlockSource for FileBlocks {
    fn revision_id(&self) -> &str {
        &self.revision_id
    }
    fn block_sizes(&self) -> &[u64] {
        &self.sizes
    }
    async fn read_block(&self, index: usize) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let (start, size) = match (self.map.start_of(index), self.map.size_of(index)) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return Err(Error::NotFound(format!(
                    "offline block {index} is past the end"
                )));
            }
        };
        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut data = vec![0; size as usize];
        file.read_exact(&mut data).await?;
        Ok(data)
    }
}
