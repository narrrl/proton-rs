CREATE TABLE source_kinds (
    source TEXT PRIMARY KEY NOT NULL REFERENCES sources(id),
    kind TEXT NOT NULL CHECK(kind IN ('photos','drive','device','other'))
) STRICT;
INSERT INTO source_kinds(source,kind)
SELECT id,CASE WHEN id='local-device' THEN 'device'
    WHEN id LIKE 'drive-%' THEN 'drive' ELSE 'other' END FROM sources;
CREATE TABLE album_nodes (
    gallery TEXT NOT NULL REFERENCES sources(id),
    source TEXT NOT NULL REFERENCES sources(id),
    parent TEXT,
    PRIMARY KEY(gallery,source),
    FOREIGN KEY(gallery,parent) REFERENCES album_nodes(gallery,source),
    CHECK(source<>parent),
    CHECK((source=gallery AND parent IS NULL) OR (source<>gallery AND parent IS NOT NULL))
) STRICT;
CREATE INDEX album_children ON album_nodes(gallery,parent,source);
CREATE TABLE media_shapes (
    account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
    width INTEGER NOT NULL CHECK(width>0 AND width<=1000000),
    height INTEGER NOT NULL CHECK(height>0 AND height<=1000000),
    PRIMARY KEY(account,volume,node),
    FOREIGN KEY(account,volume,node) REFERENCES assets(account,volume,node)
) STRICT;
