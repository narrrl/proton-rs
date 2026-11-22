# proton-rs

Shared building blocks for the Proton Drive app family built on
[proton-sdk-rs](https://github.com/narrrl/proton-sdk-rs):
[proton-drive-linux](https://github.com/narrrl/proton-drive-linux),
[Proton Gallery](https://github.com/narrrl/proton-gallery) and
[Proton Stream](https://github.com/narrrl/proton-stream).

proton-sdk-rs stays a pure port of Proton's API. Everything an *app* needs on
top of it — a local catalog, sync services, caches, shared UI tokens — lives
here, published to crates.io, so the apps depend on released versions instead
of git revisions of each other.

| Crate | What it holds |
|---|---|
| `proton-rs` | Umbrella crate; re-exports the others behind features |
| `proton-gallery-rs` | SDK-free photo library core: SQLite catalog, timeline/outline, albums, justified grid, places |
| `proton-gallery-drive-rs` | Proton Drive adapter for the gallery core (auth, Photos volume, Drive galleries) |
| `proton-gallery-app-rs` | Gallery application services: account, sync, media, video, albums, favorites, sharing, hosted mode |
| `proton-auth-rs` | Credential storage: one `SecretStore` trait, chunking for small OS stores, and an OS keyring backend (feature `keyring`) that reconnects after a Secret Service restart and replaces items instead of editing them |
| `proton-base-rs` | Plumbing: versioned SQLite migrations with an application-id guard, crash-safe atomic file writes, JSON config helpers (feature `json`). Pins the suite's `rusqlite` |
| `proton-media-rs` | Media plumbing: block range maps, single-flight fetches, a crash-safe byte-budgeted disk block cache, seekable cached byte streams |
| `proton-drive-tree-rs` | Drive folder trees: one surface over public links and account folders, a parallel retrying cycle-safe walk, and the event feed read as "which crawled trees changed" |
| `proton-map-rs` | Offline vector basemap: PMTiles reader, vector tile decoder, anti-aliased raster tile renderer with light and dark styles and town labels, tile cache. `tools/basemap` builds the archive from Natural Earth and GeoNames |

## Local development across repos

App repos never commit a `[patch.crates-io]`. To build an app against a local
checkout of this workspace, put the override in the parent directory that holds
all checkouts (cargo reads config from parent directories):

```toml
# <checkouts>/.cargo/config.toml
[patch.crates-io]
proton-gallery-rs = { path = "proton-rs/crates/proton-gallery-rs" }
proton-gallery-drive-rs = { path = "proton-rs/crates/proton-gallery-drive-rs" }
proton-gallery-app-rs = { path = "proton-rs/crates/proton-gallery-app-rs" }
proton-auth-rs = { path = "proton-rs/crates/proton-auth-rs" }
proton-base-rs = { path = "proton-rs/crates/proton-base-rs" }
proton-media-rs = { path = "proton-rs/crates/proton-media-rs" }
proton-drive-tree-rs = { path = "proton-rs/crates/proton-drive-tree-rs" }
proton-map-rs = { path = "proton-rs/crates/proton-map-rs" }
```

## Performance

Proton round trips are slow; the apps feel fast because of the local catalog
and caches. Changes here must not regress the numbers in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## License

MIT
