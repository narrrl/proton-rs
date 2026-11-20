# proton-base-rs

Plumbing shared by the Proton Drive app family
([proton-rs](https://github.com/narrrl/proton-rs)).

- **`Schema`**: forward-only SQLite migrations tracked in `PRAGMA user_version`.
  Each step (SQL or a Rust function) runs in its own transaction with its
  version bump, so an interrupted upgrade resumes where it stopped. A database
  written by a newer build is refused. With `application_id`, the schema also
  refuses another application's database and an unversioned database that
  already has tables.
- **`atomic::write`**: replace a file so a crash leaves the old or the new
  contents, never a torn or empty file. Owner-only (`0600`) on Unix by
  default. Feature `json` adds `read_json` / `write_json`.

```rust
use proton_base_rs::{Schema, Step, rusqlite::Connection};

const SCHEMA: Schema = Schema::new(&[
    Step::Sql("CREATE TABLE item (id INTEGER PRIMARY KEY, name TEXT);"),
    Step::Sql("ALTER TABLE item ADD COLUMN size INTEGER;"),
])
.application_id(0x5052_4f54);

let mut conn = Connection::open_in_memory()?;
SCHEMA.check(&conn)?; // before pragmas that write, like journal_mode=WAL
conn.execute_batch("PRAGMA journal_mode=WAL;")?;
SCHEMA.migrate(&mut conn)?;
# Ok::<(), proton_base_rs::Error>(())
```

The crate fixes the suite's `rusqlite` version: cargo allows one bundled
SQLite per build, so every app follows this crate's `rusqlite` line.

## License

MIT
