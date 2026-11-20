-- Media in a hidden album leaves the system photo picker (the picker query
-- reports it as deleted). Hiding is a change the picker must hear about, so
-- every Photos member whose hidden state may have changed gets the next
-- generation: when it joins or leaves a hidden album, and when an album is
-- hidden or shown again.
CREATE TRIGGER picker_hidden_join AFTER INSERT ON memberships
    WHEN EXISTS(SELECT 1 FROM album_settings WHERE source=NEW.source AND hidden=1)
    AND EXISTS(SELECT 1 FROM picker_changes WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node)) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node);
END;
CREATE TRIGGER picker_hidden_leave AFTER DELETE ON memberships
    WHEN EXISTS(SELECT 1 FROM album_settings WHERE source=OLD.source AND hidden=1)
    AND EXISTS(SELECT 1 FROM picker_changes WHERE (account,volume,node)=(OLD.account,OLD.volume,OLD.node)) BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node)=(OLD.account,OLD.volume,OLD.node);
END;
CREATE TRIGGER picker_hidden_insert AFTER INSERT ON album_settings
    WHEN NEW.hidden=1 BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node) IN (SELECT account,volume,node FROM memberships WHERE source=NEW.source);
END;
CREATE TRIGGER picker_hidden_update AFTER UPDATE OF hidden ON album_settings
    WHEN OLD.hidden<>NEW.hidden BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node) IN (SELECT account,volume,node FROM memberships WHERE source=NEW.source);
END;
CREATE TRIGGER picker_hidden_delete AFTER DELETE ON album_settings
    WHEN OLD.hidden=1 BEGIN
    UPDATE picker_state SET generation=generation+1;
    UPDATE picker_changes SET generation=(SELECT generation FROM picker_state)
    WHERE (account,volume,node) IN (SELECT account,volume,node FROM memberships WHERE source=OLD.source);
END;
