-- Removal from Proton Photos reaches the system photo picker too. A Photos
-- member that leaves its source (trashed here, or gone from Proton at the
-- next sync) keeps its row with deleted=1 and a new generation, so the
-- picker can ask what was deleted after the generation it last saw.
ALTER TABLE picker_changes ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0 CHECK(deleted IN (0,1));
CREATE INDEX picker_deleted_order ON picker_changes(deleted,generation);
CREATE TRIGGER picker_member_delete AFTER DELETE ON memberships
    WHEN EXISTS(SELECT 1 FROM source_kinds WHERE source=OLD.source AND kind='photos') BEGIN
    UPDATE picker_state SET generation=generation+1;
    INSERT INTO picker_changes(account,volume,node,generation,deleted)
    VALUES (OLD.account,OLD.volume,OLD.node,(SELECT generation FROM picker_state),1)
    ON CONFLICT(account,volume,node) DO UPDATE SET generation=excluded.generation,deleted=1;
END;
-- Rows are inserted with INSERT OR IGNORE, and an outer OR clause overrides
-- the one inside a trigger: v7's INSERT OR REPLACE never refreshed the
-- generation of media that joined the source again. An upsert does.
DROP TRIGGER picker_member_insert;
CREATE TRIGGER picker_member_insert AFTER INSERT ON memberships
    WHEN EXISTS(SELECT 1 FROM source_kinds WHERE source=NEW.source AND kind='photos') BEGIN
    UPDATE picker_state SET generation=generation+1;
    INSERT INTO picker_changes(account,volume,node,generation,deleted)
    VALUES (NEW.account,NEW.volume,NEW.node,(SELECT generation FROM picker_state),0)
    ON CONFLICT(account,volume,node) DO UPDATE SET generation=excluded.generation,deleted=0;
END;
