-- Proton albums that backups go into. `folder` is a phone folder (MediaStore
-- bucket), or '' for every backup. Proton uploads cannot name an album, so
-- each photo is added after its upload.
CREATE TABLE backup_albums (
    account TEXT NOT NULL,
    folder TEXT NOT NULL,
    album TEXT NOT NULL REFERENCES sources(id),
    PRIMARY KEY(account,folder,album)
) STRICT;
-- Phone media waiting to go into an album: queued when it is backed up (or
-- asked for before it is), cleared once Proton has it in the album. Survives
-- restarts, so an add that failed is tried again with the next backup.
CREATE TABLE album_adds (
    account TEXT NOT NULL,
    node TEXT NOT NULL REFERENCES phone_media(node) ON DELETE CASCADE,
    album TEXT NOT NULL REFERENCES sources(id),
    PRIMARY KEY(account,node,album)
) STRICT;
