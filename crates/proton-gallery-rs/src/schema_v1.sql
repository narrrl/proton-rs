CREATE TABLE sources (id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL) STRICT;
CREATE TABLE assets (
    account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
    sort_time INTEGER NOT NULL, payload TEXT NOT NULL,
    PRIMARY KEY(account,volume,node)
) STRICT;
CREATE INDEX assets_timeline ON assets(sort_time DESC,account DESC,volume DESC,node DESC);
CREATE TABLE memberships (
    source TEXT NOT NULL REFERENCES sources(id),
    account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
    PRIMARY KEY(source,account,volume,node),
    FOREIGN KEY(account,volume,node) REFERENCES assets(account,volume,node)
) STRICT;
CREATE TABLE scans (
    source TEXT PRIMARY KEY NOT NULL REFERENCES sources(id),
    generation INTEGER NOT NULL CHECK(generation > 0),
    cursor TEXT, complete INTEGER NOT NULL CHECK(complete IN (0,1)),
    indexed INTEGER NOT NULL CHECK(indexed >= 0)
) STRICT;
CREATE TABLE revisions (id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL) STRICT;
CREATE TABLE offline (
    revision TEXT PRIMARY KEY NOT NULL REFERENCES revisions(id),
    complete INTEGER NOT NULL CHECK(complete IN (0,1))
) STRICT;
