ALTER TABLE memberships ADD COLUMN sort_time INTEGER NOT NULL DEFAULT 0;
UPDATE memberships SET sort_time=(SELECT a.sort_time FROM assets a
    WHERE (a.account,a.volume,a.node)=(memberships.account,memberships.volume,memberships.node));
CREATE INDEX memberships_timeline ON memberships(source,sort_time DESC,account DESC,volume DESC,node DESC);
CREATE INDEX memberships_asset ON memberships(account,volume,node);
CREATE TRIGGER membership_time_insert AFTER INSERT ON memberships BEGIN
    UPDATE memberships SET sort_time=(SELECT a.sort_time FROM assets a
        WHERE (a.account,a.volume,a.node)=(NEW.account,NEW.volume,NEW.node))
    WHERE (source,account,volume,node)=(NEW.source,NEW.account,NEW.volume,NEW.node);
END;
CREATE TRIGGER asset_time_update AFTER UPDATE OF sort_time ON assets WHEN OLD.sort_time<>NEW.sort_time BEGIN
    UPDATE memberships SET sort_time=NEW.sort_time
    WHERE (account,volume,node)=(NEW.account,NEW.volume,NEW.node);
END;
