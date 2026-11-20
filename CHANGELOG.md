# Changelog

All notable changes to this workspace are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and the crates
follow semantic versioning.

## proton-base-rs [0.1.0] - Unreleased

### Added

- `Schema`: forward-only SQLite migrations in `PRAGMA user_version`, one
  transaction per step (SQL or code), refusing databases from newer builds.
  An optional `application_id` keeps an app from adopting a foreign or
  nonempty unknown database.
- `atomic::write`: crash-safe file replacement (unique temporary file, sync,
  rename, directory sync), owner-only by default; `read_json`/`write_json`
  behind feature `json`.

### Changed

- proton-gallery-rs: the catalog migrates through `Schema`. Same steps, same
  errors.

## proton-auth-rs [0.1.0] - 2026-11-20

### Added

- `SecretStore`, the credential-store interface shared by the apps, with
  `MemoryStore` and `Chunked` (splits values too long for one Windows
  credential, compatible with what Proton Stream already stored).
- `KeyringStore` (feature `keyring`) over `keyring-core`: Secret Service,
  Windows Credential Manager and macOS Keychain. Reads entries written by
  `keyring` 3. Reconnects once after a platform failure, and on the Secret
  Service replaces an item instead of editing its secret (moved from
  proton-drive-linux, where oo7 before 0.7.0 lost edited secrets on reboot).
  `set_bytes`/`get_bytes` for secrets that are not text.

## proton-gallery-rs [0.1.1] - 2026-11-20

### Added

- `grouping`: classify files as photo, video or camera raw, and group the
  files of one shot (RAW+JPEG pairs, Pixel raw captures and their covers, the
  server's live-photo relation) under one representative.
- `takeout`: read a Google Photos Takeout export (albums, capture times,
  locations, favorites, live photos) from its zip set without extracting it.
- `exiftime`: capture time from EXIF, honouring the offset tags.
- `similar`: perceptual difference hash for finding look-alike photos.

All four moved from proton-drive-linux, which now uses them from here, so
Proton Gallery can offer the same grouping and Takeout import.

## [0.1.0] - 2026-11-20

### Added

- `proton-gallery-rs`, `proton-gallery-drive-rs` and `proton-gallery-app-rs`,
  moved unchanged from Proton Gallery's `pg-core`, `pg-drive` and `pg-app`
  (proton-gallery commit b171232). proton-drive-linux pinned these by git
  revision; publishing them lets both apps use normal semver dependencies.
- `proton-rs` umbrella crate re-exporting them behind features.
