-- Change order of Proton Photos media, for Android's system photo picker.
-- Every new or changed member of a Photos source gets the next generation,
-- so the picker can ask for "everything changed after N" instead of reading
-- the whole library again. The collection identifier is random per catalog:
-- a rebuilt catalog starts counting again, and the picker must notice.
CREATE TABLE picker_state (
    id INTEGER PRIMARY KEY CHECK(id=1),
    collection TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation>=0)
) STRICT;
CREATE TABLE picker_changes (
    account TEXT NOT NULL, volume TEXT NOT NULL, node TEXT NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY(account,volume,node)
) STRICT;
CREATE INDEX picker_changes_order ON picker_changes(generation);

INSERT OR IGNORE INTO picker_changes(account,volume,node,generation)
SELECT m.account,m.volume,m.node,row_number() OVER (ORDER BY m.sort_time,m.account,m.volume,m.node)
FROM memberships m JOIN source_kinds k ON k.source=m.source AND k.kind='photos';
INSERT INTO picker_state(id,collection,generation)
SELECT 1,lower(hex(randomblob(16))),coalesce(max(generation),0) FROM picker_changes;

CREATE TRIGGER picker_member_insert AFTER INSERT ON memberships
    WHEN EXISTS(SELECT 1 FROM source_kinds WHERE source=NEW.source AND kind='photos') BEGIN
    UPDATE picker_state SET generation=generation+1;
    INSERT OR REPLACE INTO picker_changes(account,volume,node,generation)
    VALUES (NEW.account,NEW.volume,NEW.node,(SELECT generation FROM picker_state));
END;
CREATE TRIGGER picker_asset_update AFTER UPDATE OF payload,sort_time ON assets
    WHEN (OLD.payload IS NOT NEW.payload OR OLD.sort_time<>NEW.sort_time)
    AND EXISTS(SELECT 1 FROM picker_changes WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node)) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node);
END;
-- The picker hides a Proton photo while the same file is on the phone, so a
-- phone copy appearing or going away (backed up, freed up) is a change too.
CREATE TRIGGER picker_copy_insert AFTER INSERT ON phone_backup
    WHEN NEW.state='backed_up'
    AND EXISTS(SELECT 1 FROM picker_changes WHERE (account,volume,node)=(NEW.account,NEW.remote_volume,NEW.remote_node)) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node)=(NEW.account,NEW.remote_volume,NEW.remote_node);
END;
CREATE TRIGGER picker_copy_update AFTER UPDATE ON phone_backup
    WHEN (OLD.remote_volume IS NOT NEW.remote_volume OR OLD.remote_node IS NOT NEW.remote_node) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node) IN (VALUES (OLD.account,OLD.remote_volume,OLD.remote_node),(NEW.account,NEW.remote_volume,NEW.remote_node));
END;
CREATE TRIGGER picker_copy_delete AFTER DELETE ON phone_backup
    WHEN OLD.state='backed_up'
    AND EXISTS(SELECT 1 FROM picker_changes WHERE (account,volume,node)=(OLD.account,OLD.remote_volume,OLD.remote_node)) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node)=(OLD.account,OLD.remote_volume,OLD.remote_node);
END;
