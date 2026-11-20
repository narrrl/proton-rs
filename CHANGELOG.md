# Changelog

All notable changes to this workspace are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and the crates
follow semantic versioning.

## [0.1.0] - 2026-11-20

### Added

- `proton-gallery-rs`, `proton-gallery-drive-rs` and `proton-gallery-app-rs`,
  moved unchanged from Proton Gallery's `pg-core`, `pg-drive` and `pg-app`
  (proton-gallery commit b171232). proton-drive-linux pinned these by git
  revision; publishing them lets both apps use normal semver dependencies.
- `proton-rs` umbrella crate re-exporting them behind features.
