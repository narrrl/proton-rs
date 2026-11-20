//! Forward-only schema migrations.
//!
//! Step *n* (1-based) moves the schema from version *n − 1* to *n*. Steps are
//! never edited once shipped: a released version is already on disk somewhere,
//! so changing it would silently diverge from what an existing install has.
//! Add a new step instead.

use rusqlite::{Connection, Transaction};

use crate::{Error, Result};

/// One migration step.
pub enum Step {
    /// SQL run with `execute_batch`.
    Sql(&'static str),
    /// Code, for a step SQL alone cannot express (a conditional rewrite,
    /// a backfill computed in Rust).
    Run(fn(&Transaction<'_>) -> rusqlite::Result<()>),
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(sql) => f.debug_tuple("Sql").field(sql).finish(),
            Self::Run(_) => f.write_str("Run(..)"),
        }
    }
}

/// What [`Schema::migrate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migrated {
    /// The version found on disk.
    pub from: u32,
    /// The version now on disk: always [`Schema::version`].
    pub to: u32,
}

impl Migrated {
    /// Whether any step ran.
    pub fn changed(&self) -> bool {
        self.from != self.to
    }
}

/// An ordered list of migration steps.
#[derive(Debug, Clone, Copy)]
pub struct Schema {
    steps: &'static [Step],
    application_id: Option<u32>,
}

impl Schema {
    pub const fn new(steps: &'static [Step]) -> Self {
        Self {
            steps,
            application_id: None,
        }
    }

    /// Claim the database with `PRAGMA application_id`.
    ///
    /// The first step writes `id`. From then on the schema refuses a database
    /// carrying a different id, a versioned database without it, and an
    /// unversioned database that already has tables.
    pub const fn application_id(mut self, id: u32) -> Self {
        self.application_id = Some(id);
        self
    }

    /// The version this schema migrates to: the number of steps.
    pub fn version(&self) -> u32 {
        u32::try_from(self.steps.len()).expect("fewer than 2^32 migration steps")
    }

    /// Check that `conn` can be opened under this schema without changing it,
    /// and return the version on disk.
    ///
    /// Run this before any pragma that writes to the file (`journal_mode=WAL`
    /// rewrites the header), so a refused database is left untouched.
    pub fn check(&self, conn: &Connection) -> Result<u32> {
        let raw: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let version = u32::try_from(raw).map_err(|_| Error::InvalidVersion(raw))?;
        if version > self.version() {
            return Err(Error::NewerSchema {
                found: version,
                supported: self.version(),
            });
        }
        let Some(expected) = self.application_id else {
            return Ok(version);
        };
        // SQLite stores the id as a signed 32-bit integer.
        let id: i64 = conn.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let id = id as u32;
        if id != 0 && id != expected {
            return Err(Error::ForeignDatabase { found: id });
        }
        if version == 0 {
            let tables: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_master \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )?;
            if tables != 0 {
                return Err(Error::NotEmpty);
            }
        } else if id != expected {
            return Err(Error::MissingApplicationId);
        }
        Ok(version)
    }

    /// Bring `conn` up to [`Schema::version`].
    ///
    /// Each step runs in its own transaction together with its
    /// `user_version` bump, so an interrupted upgrade resumes at the first
    /// step that did not commit.
    pub fn migrate(&self, conn: &mut Connection) -> Result<Migrated> {
        let from = self.check(conn)?;
        for (index, step) in self.steps.iter().enumerate().skip(from as usize) {
            let version = index as u32 + 1;
            let tx = conn.transaction()?;
            match step {
                Step::Sql(sql) => tx.execute_batch(sql)?,
                Step::Run(run) => run(&tx)?,
            }
            if version == 1
                && let Some(id) = self.application_id
            {
                tx.pragma_update(None, "application_id", id as i32)?;
            }
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        Ok(Migrated {
            from,
            to: self.version(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEPS: &[Step] = &[
        Step::Sql("CREATE TABLE a (x INTEGER);"),
        Step::Sql("ALTER TABLE a ADD COLUMN y INTEGER;"),
        Step::Run(|tx| {
            tx.execute("INSERT INTO a (x, y) VALUES (1, 2)", [])?;
            Ok(())
        }),
    ];
    const SCHEMA: Schema = Schema::new(STEPS);
    const OWNED: Schema = Schema::new(STEPS).application_id(0x5052_5354);

    fn version(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn fresh_database_runs_every_step() {
        let mut conn = Connection::open_in_memory().unwrap();
        let migrated = SCHEMA.migrate(&mut conn).unwrap();
        assert_eq!(migrated, Migrated { from: 0, to: 3 });
        assert!(migrated.changed());
        assert_eq!(version(&conn), 3);
        let y: i64 = conn.query_row("SELECT y FROM a", [], |r| r.get(0)).unwrap();
        assert_eq!(y, 2);
    }

    #[test]
    fn current_database_is_left_alone() {
        let mut conn = Connection::open_in_memory().unwrap();
        SCHEMA.migrate(&mut conn).unwrap();
        let again = SCHEMA.migrate(&mut conn).unwrap();
        assert_eq!(again, Migrated { from: 3, to: 3 });
        assert!(!again.changed());
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM a", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the data step must not run twice");
    }

    #[test]
    fn resumes_after_the_last_committed_step() {
        let mut conn = Connection::open_in_memory().unwrap();
        Schema::new(&STEPS[..1]).migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), 1);
        let migrated = SCHEMA.migrate(&mut conn).unwrap();
        assert_eq!(migrated.from, 1);
        assert_eq!(version(&conn), 3);
    }

    #[test]
    fn failing_step_rolls_back_only_itself() {
        const BROKEN: &[Step] = &[
            Step::Sql("CREATE TABLE a (x INTEGER);"),
            Step::Sql("CREATE TABLE b (x INTEGER); THIS IS NOT SQL;"),
        ];
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(Schema::new(BROKEN).migrate(&mut conn).is_err());
        assert_eq!(version(&conn), 1);
        let b: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(b, 0);
    }

    #[test]
    fn newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 9).unwrap();
        let err = SCHEMA.migrate(&mut conn).unwrap_err();
        assert!(matches!(
            err,
            Error::NewerSchema {
                found: 9,
                supported: 3
            }
        ));
    }

    #[test]
    fn negative_version_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", -1).unwrap();
        assert!(matches!(
            SCHEMA.check(&conn),
            Err(Error::InvalidVersion(-1))
        ));
    }

    #[test]
    fn application_id_is_written_with_the_first_step() {
        let mut conn = Connection::open_in_memory().unwrap();
        OWNED.migrate(&mut conn).unwrap();
        let id: i64 = conn
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(id, 0x5052_5354);
        OWNED.migrate(&mut conn).unwrap();
    }

    #[test]
    fn high_bit_application_id_round_trips() {
        let schema = Schema::new(STEPS).application_id(0xF000_0001);
        let mut conn = Connection::open_in_memory().unwrap();
        schema.migrate(&mut conn).unwrap();
        assert_eq!(schema.check(&conn).unwrap(), 3);
    }

    #[test]
    fn foreign_database_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "application_id", 7).unwrap();
        assert!(matches!(
            OWNED.check(&conn),
            Err(Error::ForeignDatabase { found: 7 })
        ));
    }

    #[test]
    fn unversioned_database_with_tables_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE theirs (x);").unwrap();
        assert!(matches!(OWNED.check(&conn), Err(Error::NotEmpty)));
        // Without an application id the schema does not claim the file.
        assert_eq!(SCHEMA.check(&conn).unwrap(), 0);
    }

    #[test]
    fn versioned_database_without_our_id_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        SCHEMA.migrate(&mut conn).unwrap();
        assert!(matches!(
            OWNED.check(&conn),
            Err(Error::MissingApplicationId)
        ));
    }
}
