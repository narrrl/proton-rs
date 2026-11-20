-- Proton Photos albums are sources of their own kind. The kind check is part
-- of the table, so the table is rebuilt with 'album' allowed.
-- Triggers name source_kinds; legacy renaming leaves them to find the new
-- table under the old name instead of rejecting the rename.
PRAGMA legacy_alter_table=ON;
CREATE TABLE source_kinds_v9 (
    source TEXT PRIMARY KEY NOT NULL REFERENCES sources(id),
    kind TEXT NOT NULL CHECK(kind IN ('photos','drive','device','album','other'))
) STRICT;
INSERT INTO source_kinds_v9(source,kind) SELECT source,kind FROM source_kinds;
DROP TABLE source_kinds;
ALTER TABLE source_kinds_v9 RENAME TO source_kinds;
PRAGMA legacy_alter_table=OFF;
