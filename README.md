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
```

## Performance

Proton round trips are slow; the apps feel fast because of the local catalog
and caches. Changes here must not regress the numbers in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## License

MIT
