-- Presentation choices for any source (Drive album, Photos timeline, device
-- library). Keyed by source, so an album shared by overlapping galleries keeps
-- one name and cover. Nothing here is written back to Proton.
CREATE TABLE album_settings (
    source TEXT PRIMARY KEY NOT NULL REFERENCES sources(id),
    title TEXT CHECK(title IS NULL OR length(title) BETWEEN 1 AND 200),
    description TEXT CHECK(description IS NULL OR length(description) BETWEEN 1 AND 2000),
    cover_account TEXT,
    cover_volume TEXT,
    cover_node TEXT,
    grouping TEXT NOT NULL DEFAULT 'day' CHECK(grouping IN ('day','month','year','none')),
    sort_order TEXT NOT NULL DEFAULT 'newest' CHECK(sort_order IN ('newest','oldest','name','name_desc')),
    pinned INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0,1)),
    hidden INTEGER NOT NULL DEFAULT 0 CHECK(hidden IN (0,1)),
    CHECK((cover_account IS NULL) = (cover_volume IS NULL) AND (cover_volume IS NULL) = (cover_node IS NULL))
) STRICT;
CREATE TABLE album_tags (
    source TEXT NOT NULL REFERENCES sources(id),
    tag TEXT NOT NULL COLLATE NOCASE CHECK(length(tag) BETWEEN 1 AND 40),
    PRIMARY KEY(source,tag)
) STRICT;
CREATE INDEX album_tags_by_tag ON album_tags(tag COLLATE NOCASE,source);

-- Name order. Memberships carry the name like they carry sort_time, so an
-- album pages by name through an index. Case is ignored.
ALTER TABLE assets ADD COLUMN sort_name TEXT COLLATE NOCASE
    GENERATED ALWAYS AS (coalesce(json_extract(payload,'$.name'),'')) VIRTUAL;
CREATE INDEX assets_by_name ON assets(sort_name COLLATE NOCASE,account,volume,node);
ALTER TABLE memberships ADD COLUMN sort_name TEXT NOT NULL DEFAULT '' COLLATE NOCASE;
UPDATE memberships SET sort_name=(SELECT a.sort_name FROM assets a
    WHERE (a.account,a.volume,a.node)=(memberships.account,memberships.volume,memberships.node));
CREATE INDEX memberships_by_name ON memberships(source,sort_name COLLATE NOCASE,account,volume,node);
CREATE TRIGGER membership_name_insert AFTER INSERT ON memberships BEGIN
    UPDATE memberships SET sort_name=(SELECT a.sort_name FROM assets a
        WHERE (a.account,a.volume,a.node)=(NEW.account,NEW.volume,NEW.node))
    WHERE (source,account,volume,node)=(NEW.source,NEW.account,NEW.volume,NEW.node);
END;
CREATE TRIGGER asset_name_update AFTER UPDATE OF payload ON assets
    WHEN OLD.sort_name IS NOT NEW.sort_name BEGIN
    UPDATE memberships SET sort_name=NEW.sort_name
    WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node);
END;
