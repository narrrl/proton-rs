# Benchmarks

Baseline recorded 2026-11-20 before the extraction, with Proton Gallery's CLI
(`pg-cli`, release build) at proton-gallery b171232, on the maintainer's
Linux workstation. Re-run after any change to the catalog, sync or media paths;
a regression blocks the merge.

```sh
cargo build --release -p pg-cli
B=target/release/proton-gallery D=$(mktemp -d)
$B --data-dir $D demo --count 50000          # index a synthetic library
$B --data-dir $D benchmark --queries 1000 --limit 200   # timeline pages
```

| Measure | Baseline |
|---|---|
| Index 50,000 synthetic assets | 2,549 ms |
| Timeline page of 200, p50 | 349–351 µs |
| Timeline page of 200, p95 | 394–540 µs |
| Timeline page of 200, p99 | 511–834 µs |

Desktop view construction can be timed with `PG_METRICS=1` (prints
`PG_METRICS view_us=` per frame).

## After the extraction (0.1.0)

Proton Gallery built against these crates (branch `refactor/proton-rs`),
compared with the b171232 build interleaved run by run under the same machine
load (load average 6–7, so absolute numbers sit above the idle baseline):

| Run | Before: index / p50 / p95 | After: index / p50 / p95 |
|---|---|---|
| 1 | 3,258 ms / 624 µs / 857 µs | 4,563 ms / 503 µs / 1,039 µs |
| 2 | 2,863 ms / 579 µs / 891 µs | 2,954 ms / 506 µs / 1,061 µs |
| 3 | 3,149 ms / 484 µs / 886 µs | 2,986 ms / 523 µs / 646 µs |

No difference beyond noise: the move changed crate names only. Compare
interleaved like this on a loaded machine; one-off runs are not comparable.
