CREATE TABLE photo_locations (
    account TEXT NOT NULL,
    volume TEXT NOT NULL,
    node TEXT NOT NULL,
    revision TEXT,
    latitude REAL,
    longitude REAL,
    PRIMARY KEY(account,volume,node),
    FOREIGN KEY(account,volume,node) REFERENCES assets(account,volume,node) ON DELETE CASCADE
) STRICT;
