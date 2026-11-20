-- Immutable, account-owned settings operations. An outgoing operation stays
-- pending until its exact bytes have been uploaded or read back from Proton.
CREATE TABLE gallery_configuration (
    account TEXT NOT NULL,
    id TEXT NOT NULL,
    document TEXT NOT NULL CHECK(json_valid(document)),
    pending INTEGER NOT NULL CHECK(pending IN (0,1)),
    PRIMARY KEY(account,id)
) STRICT;

CREATE TABLE gallery_configuration_imported (
    account TEXT PRIMARY KEY
) STRICT;

-- A fresh client keeps Proton media private until it completes a visibility
-- read and materializes membership for every hidden Photos album.
CREATE TABLE gallery_visibility_verified (
    account TEXT PRIMARY KEY
) STRICT;

CREATE TABLE gallery_event_cursors (
    account TEXT NOT NULL,
    volume TEXT NOT NULL,
    cursor TEXT NOT NULL,
    PRIMARY KEY(account,volume)
) STRICT;
