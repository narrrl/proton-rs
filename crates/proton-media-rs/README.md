# proton-media-rs

Media plumbing shared by the Proton Drive app family
([proton-rs](https://github.com/narrrl/proton-rs)). SDK-free: it works on block
sizes, byte ranges and string identities, not on Proton types.

- **`BlockMap`, `splice`**: resolve a byte range of a file stored as blocks of
  known, possibly uneven, sizes to the blocks that overlap it, and copy each
  block's part into the caller's buffer. Never assumes a fixed block size.
- **`SingleFlight`**: one in-flight call per key. Concurrent callers wait for
  the leader's result; a cancelled or failed leader never wedges the key.
- **`DiskCache`**: a byte-budgeted LRU block cache on disk. A block is synced
  before its length sidecar is written, so a torn entry reads as a miss, never
  as short data. Paths are `sha256(file + revision)`, so names never reach the
  disk and a new revision never reads an old one's bytes. Entries left by a
  previous run are adopted on open.
- **`SingleFlight::join`**: the same for values that carry their own outcome:
  every caller gets the leader's typed result, and a waiter whose leader is
  cancelled does the work itself.
- **`ImageCache`**: the same disk guarantees for encoded pictures (thumbnails,
  previews, posters), keyed by source, version and variant.
- **`stream`**: seekable byte streams over block-stored files. A shared,
  byte-budgeted memory ring in front of the disk cache, single-flight block
  fetches, read-ahead that a seek cancels (measured: roughly 5x on worst-case
  seek latency), and an LRU of open streams. Generic over the file key. Feature
  `drive` adds `RevisionBlocks`, a Proton Drive revision as the block source.
- **`render`** (feature `image`): decode with limits, apply EXIF orientation,
  shrink to fit, re-encode as JPEG (PNG when transparent).

```rust
use proton_media_rs::BlockMap;

let map = BlockMap::new(&[4096, 4096, 1000]);
assert_eq!(map.size(), 9192);
assert_eq!(map.blocks_in(4095, 4097), 0..2);
```

## License

MIT
