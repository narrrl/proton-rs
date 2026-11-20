# proton-auth-rs

Credential storage for the Proton Drive app family (proton-drive-linux,
Proton Gallery, Proton Stream).

- `SecretStore`: the one interface a signed-in session and other secrets are
  kept behind. Android and iOS apps implement it over their platform keystore.
- `KeyringStore` (feature `keyring`): the OS credential store. Secret Service
  on Linux and the BSDs, Credential Manager on Windows, the login Keychain on
  macOS. It reads entries written by `keyring` 3, so moving an app over keeps
  its users signed in.
  - It reconnects once when the Secret Service fails the process's connection,
    for example after the provider restarted.
  - On the Secret Service, a changed secret is written as a new item that
    replaces the old one, because oo7 before 0.7.0 keeps an edited secret only
    in memory.
- `Chunked`: splits a value too long for one entry (Windows caps an entry at
  1280 UTF-16 units) across several.
- `MemoryStore`: secrets for this process only.

```rust
use proton_auth_rs::{Chunked, KeyringStore, SecretStore};

let secrets = Chunked::new(KeyringStore::new("my-app"));
secrets.set("session", "{...}")?;
assert!(secrets.get("session")?.is_some());
# Ok::<(), proton_auth_rs::Error>(())
```

License: MIT
