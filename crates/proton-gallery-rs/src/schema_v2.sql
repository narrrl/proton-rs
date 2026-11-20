CREATE TABLE local_media (
    account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
    content_hash TEXT NOT NULL CHECK(length(content_hash)=64),
    size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
    PRIMARY KEY(account,volume,node),
    FOREIGN KEY(account,volume,node) REFERENCES assets(account,volume,node)
) STRICT;
