-- The phone's own photo library (Android MediaStore). Metadata only: the files
-- stay where the system keeps them. Each row also has an asset under account
-- 'phone' and a membership of source 'phone', so the merged Photos timeline
-- pages phone and Proton media with one keyset.
CREATE TABLE phone_media (
    node TEXT PRIMARY KEY NOT NULL,
    folder TEXT NOT NULL,
    folder_name TEXT NOT NULL,
    modified INTEGER NOT NULL,
    size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
    favorite INTEGER NOT NULL DEFAULT 0 CHECK(favorite IN (0,1)),
    -- Plaintext SHA-1, lowercase hex: what Proton's duplicate check matches.
    sha1 TEXT CHECK(sha1 IS NULL OR length(sha1)=40),
    scan INTEGER NOT NULL
) STRICT;
CREATE INDEX phone_media_folder ON phone_media(folder);

-- Whether an account holds a copy. 'missing': not in Proton. 'maybe': a
-- photo of the same name exists, contents not compared yet. 'backed_up': the
-- remote photo is known. Cleared when the phone file changes.
CREATE TABLE phone_backup (
    account TEXT NOT NULL,
    node TEXT NOT NULL REFERENCES phone_media(node) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK(state IN ('missing','maybe','backed_up')),
    remote_volume TEXT,
    remote_node TEXT,
    PRIMARY KEY(account,node),
    CHECK((state='backed_up') = (remote_node IS NOT NULL AND remote_volume IS NOT NULL))
) STRICT;
CREATE INDEX phone_backup_remote ON phone_backup(account,remote_volume,remote_node);

-- Phone folders (MediaStore buckets) whose new media upload automatically.
CREATE TABLE backup_folders (folder TEXT PRIMARY KEY NOT NULL) STRICT;
