CREATE TABLE photos_sync (
    source TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
    cursor TEXT,
    listing INTEGER NOT NULL DEFAULT 1 CHECK(listing BETWEEN 0 AND 2),
    replay INTEGER NOT NULL DEFAULT 0,
    albums_pending INTEGER NOT NULL DEFAULT 1,
    last_success INTEGER,
    revision INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE photos_pending (
    source TEXT NOT NULL REFERENCES photos_sync(source) ON DELETE CASCADE,
    node TEXT NOT NULL,
    capture_time INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(source,node)
) STRICT;
CREATE INDEX photos_pending_order ON photos_pending(source,attempts,node);
CREATE TABLE photos_removed (
    account TEXT NOT NULL,
    volume TEXT NOT NULL,
    node TEXT NOT NULL,
    PRIMARY KEY(account,volume,node)
) STRICT;
CREATE TABLE photos_sync_active (
    account TEXT PRIMARY KEY,
    source TEXT NOT NULL REFERENCES photos_sync(source) ON DELETE CASCADE
) STRICT;
CREATE TABLE photos_album_pending (
    source TEXT NOT NULL REFERENCES photos_sync(source) ON DELETE CASCADE,
    album TEXT NOT NULL,
    node TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(source,album)
) STRICT;
CREATE INDEX photos_album_pending_order ON photos_album_pending(source,attempts,album);
