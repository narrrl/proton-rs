#![doc = include_str!("../README.md")]

pub mod atomic;
mod schema;

pub use rusqlite;
pub use schema::{Migrated, Schema, Step};

#[cfg(feature = "json")]
pub use atomic::{read_json, write_json};

/// Errors from this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A newer build wrote this database. Reading it under older rules could
    /// silently mangle it, so it is refused instead.
    #[error("database schema {found} is newer than this build supports ({supported})")]
    NewerSchema { found: u32, supported: u32 },
    /// `PRAGMA user_version` is negative, which no build of ours writes.
    #[error("database schema version {0} is invalid")]
    InvalidVersion(i64),
    /// The database carries another application's `application_id`.
    #[error("database belongs to another application (application id {found:#x})")]
    ForeignDatabase { found: u32 },
    /// A database without a schema version already has tables: it is not
    /// ours to initialize.
    #[error("refusing to initialize a nonempty unknown database")]
    NotEmpty,
    /// The database has a schema version but not our `application_id`.
    #[error("database application identifier is missing")]
    MissingApplicationId,
    #[cfg(feature = "json")]
    #[error("{path} is not valid JSON: {source}")]
    Json {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
