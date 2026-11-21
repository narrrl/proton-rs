//! A thread-safe least-recently-used cache of rendered tiles, bounded by the
//! bytes their pixels take.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use crate::{Style, Tile, TileId};

/// What a cached tile is keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub tile: TileId,
    /// The render scale's bit pattern.
    pub scale_bits: u32,
    pub style_id: u32,
}

impl CacheKey {
    pub fn new(tile: TileId, scale: f32, style: &Style) -> Self {
        Self {
            tile,
            scale_bits: scale.to_bits(),
            style_id: style.id,
        }
    }
}

#[derive(Default)]
struct Inner {
    tiles: HashMap<CacheKey, (u64, Arc<Tile>)>,
    /// Tiles by the tick they were last used at, oldest first.
    order: BTreeMap<u64, CacheKey>,
    tick: u64,
    bytes: usize,
}

/// Rendered tiles up to a byte budget; the least recently used go first.
pub struct TileCache {
    budget: usize,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for TileCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileCache")
            .field("budget", &self.budget)
            .field("bytes", &self.bytes())
            .finish()
    }
}

impl TileCache {
    /// A cache holding at most `budget` bytes of pixels.
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The tile under `key`, marking it as recently used.
    pub fn get(&self, key: &CacheKey) -> Option<Arc<Tile>> {
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        let (used, tile) = inner.tiles.get_mut(key)?;
        let old = std::mem::replace(used, tick);
        let tile = tile.clone();
        inner.order.remove(&old);
        inner.order.insert(tick, *key);
        Some(tile)
    }

    /// Keep `tile` under `key`, evicting the least recently used tiles
    /// beyond the budget. A tile larger than the whole budget isn't kept.
    pub fn insert(&self, key: CacheKey, tile: Arc<Tile>) {
        let size = tile.rgba.len();
        if size > self.budget {
            return;
        }
        let mut inner = self.lock();
        inner.tick += 1;
        let tick = inner.tick;
        if let Some((old, previous)) = inner.tiles.insert(key, (tick, tile)) {
            inner.order.remove(&old);
            inner.bytes -= previous.rgba.len();
        }
        inner.order.insert(tick, key);
        inner.bytes += size;
        while inner.bytes > self.budget {
            let Some((_, oldest)) = inner.order.pop_first() else {
                break;
            };
            if let Some((_, tile)) = inner.tiles.remove(&oldest) {
                inner.bytes -= tile.rgba.len();
            }
        }
    }

    /// The cached tile, or one `render` makes and the cache keeps. Two
    /// threads asking for the same missing tile may both render it.
    pub fn get_or_render(
        &self,
        tile: TileId,
        scale: f32,
        style: &Style,
        render: impl FnOnce(TileId, f32) -> Tile,
    ) -> Arc<Tile> {
        let key = CacheKey::new(tile, scale, style);
        if let Some(tile) = self.get(&key) {
            return tile;
        }
        let rendered = Arc::new(render(tile, scale));
        self.insert(key, rendered.clone());
        rendered
    }

    /// Bytes of pixels held.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn len(&self) -> usize {
        self.lock().tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        *self.lock() = Inner::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(bytes: usize) -> Arc<Tile> {
        Arc::new(Tile {
            width: 1,
            height: 1,
            rgba: vec![0; bytes],
        })
    }

    #[test]
    fn least_recently_used_tiles_go_first() {
        let cache = TileCache::new(300);
        let style = Style::light();
        let key = |x| CacheKey::new(TileId::new(3, x, 0), 1.0, &style);
        cache.insert(key(0), tile(100));
        cache.insert(key(1), tile(100));
        cache.insert(key(2), tile(100));
        assert!(cache.get(&key(0)).is_some());
        cache.insert(key(3), tile(100));
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(0)).is_some());
        assert_eq!(cache.bytes(), 300);
        cache.insert(key(4), tile(1000));
        assert!(cache.get(&key(4)).is_none());
        assert_eq!(cache.len(), 3);
        let dark = CacheKey::new(TileId::new(3, 0, 0), 1.0, &Style::dark());
        assert!(cache.get(&dark).is_none());
    }
}
