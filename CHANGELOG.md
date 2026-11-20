# Changelog

All notable changes to this workspace are documented here. The format follows
[Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/) and the crates
follow semantic versioning.

## proton-gallery-drive-rs [0.1.2] - Unreleased

### Changed

- Signing in runs through `proton_sdk::login` (proton-sdk 0.12.2), where the
  sequence from `proton_auth_rs::session` now lives. Same stored session, same
  messages, still never Proton's own text. The crate no longer depends on
  proton-auth-rs.

## proton-auth-rs [0.1.2] - Unreleased

### Deprecated

- The `session` module. Its sign-in steps moved into the SDK as
  `proton_sdk::login` (proton-sdk 0.12.2): `begin`, `Step`, `Pending`,
  `Unlocked` and `LoginError` (was `SignInError`). `SessionTokens` is replaced
  by the SDK's `ResumeParameters`, built with
  `ProtonApiSession::resume_parameters`; the password-mode numbers by
  `PasswordMode::to_wire`/`from_wire`; `verification_token` by
  `HumanVerification::token_from_message`. The module stays, unchanged, until
  0.2.0 removes it.

## proton-gallery-drive-rs [0.1.1] - 2026-11-20

### Changed

- Signing in runs through `proton_auth_rs::session`, the sequence the other
  apps share. Same stored session, same messages, and still never Proton's
  own text. A failed key unlock can still be retried on the same session.

## proton-auth-rs [0.1.1] - 2026-11-20

### Added

- `session` (feature `session`): one copy of the sign-in sequence the apps
  each had: `begin` (with an optional CAPTCHA credential),
  `Pending::second_factor`, then `Pending::mailbox_password` or
  `Pending::unlock` (both retryable on the same session), giving `Unlocked`
  with the key passphrases and salts. `SignInError` separates
  CAPTCHA gates, security-key-only accounts, blocked sign-ins, refused
  passwords, wrong codes and wrong mailbox passwords, and keeps Proton's
  response on the first three for apps that word them themselves. `SessionTokens` with
  `capture` and `resume_parameters`, the password-mode wire mapping, and
  `verification_token` for Proton's verification page messages.

## proton-media-rs [0.1.1] - 2026-11-20

### Added

- `ImageCache`: persistent picture cache (thumbnails, previews, posters) on
  the `DiskCache` guarantees, keyed by source, version and variant.
- `render` (feature `image`): decode with limits, EXIF orientation, fit to a
  box, encode as JPEG or PNG. Replaces three per-app copies.
- `SingleFlight::join`: shares the leader's typed result; a waiter whose
  leader is cancelled runs the work itself (proton-drive-linux's block fetch).
- `Error::Image`.

## proton-media-rs [0.1.0] - 2026-11-20

### Added

- `BlockMap` and `splice`: byte ranges over blocks of known, possibly uneven,
  sizes.
- `SingleFlight`: one in-flight call per key; waiters get the leader's result
  or its error message (`Shared`), and a cancelled leader never wedges a key.
- `DiskCache`: byte-budgeted LRU block cache on disk with a length sidecar
  written after the synced block, hashed paths and adoption of a previous
  run's entries.

All three moved from Proton Stream's `pstr-stream`, unchanged in behaviour and
on-disk layout.

## proton-base-rs [0.1.0] - 2026-11-20

### Added

- `Schema`: forward-only SQLite migrations in `PRAGMA user_version`, one
  transaction per step (SQL or code), refusing databases from newer builds.
  An optional `application_id` keeps an app from adopting a foreign or
  nonempty unknown database.
- `atomic::write`: crash-safe file replacement (unique temporary file, sync,
  rename, directory sync), owner-only by default; `read_json`/`write_json`
  behind feature `json`.


## proton-gallery-rs [0.1.2] - 2026-11-20

### Changed

- The catalog migrates through `proton_base_rs::Schema`. Same steps, same
  application id, same errors.

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
