//! Crash-safe file replacement.
//!
//! The new contents go to a uniquely named temporary file next to the target,
//! are synced, and are renamed over the target; then the directory is synced
//! so the rename itself survives a crash. A reader sees the old file or the
//! new one, never a torn or empty one.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// File mode for [`write`]: owner read/write only. Config and state files of
/// these apps name accounts and shares, which other local users have no
/// business reading.
pub const PRIVATE_MODE: u32 = 0o600;

/// Replace `path` with `contents`, creating parent directories as needed.
///
/// On Unix the file gets [`PRIVATE_MODE`].
pub fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    write_with_mode(path, contents, PRIVATE_MODE)
}

/// [`write`] with an explicit Unix file mode. Ignored on other platforms.
pub fn write_with_mode(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let parent = parent_of(path);
    fs::create_dir_all(parent)?;
    let temp = temp_path(path, parent);
    let result = write_temp(&temp, contents, mode).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result?;
    sync_dir(parent)
}

fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Unique per process and call, so concurrent writers of one target never
/// share a temporary file; the leading dot keeps it out of directory listings.
fn temp_path(path: &Path, parent: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn write_temp(temp: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(temp)?;
    file.write_all(contents)?;
    // Before the rename, so the rename cannot publish an empty file.
    file.sync_all()
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file; NTFS journals the rename.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Read JSON from `path`; `None` when the file does not exist.
#[cfg(feature = "json")]
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> crate::Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| crate::Error::Json {
            path: path.to_owned(),
            source,
        })
}

/// Write `value` to `path` as pretty JSON with [`write`].
#[cfg(feature = "json")]
pub fn write_json<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> crate::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| crate::Error::Json {
        path: path.to_owned(),
        source,
    })?;
    Ok(write(path, &bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp-"))
            .collect()
    }

    #[test]
    fn creates_parents_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/config.json");
        write(&path, b"one").unwrap();
        write(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert!(leftovers(path.parent().unwrap()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        write(&path, b"x").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        write_with_mode(&path, b"y", 0o644).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644 & !umask_bits());
    }

    #[cfg(unix)]
    fn umask_bits() -> u32 {
        // Probe instead of calling umask(2), which this crate cannot do safely.
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("probe");
        write_with_mode(&probe, b"", 0o777).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        0o777 & !fs::metadata(&probe).unwrap().permissions().mode()
    }

    #[test]
    fn failed_rename_leaves_target_and_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        // A directory in the way makes the rename fail.
        let path = dir.path().join("taken");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("inside"), b"keep").unwrap();
        assert!(write(&path, b"new").is_err());
        assert_eq!(fs::read(path.join("inside")).unwrap(), b"keep");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn concurrent_writers_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared");
        std::thread::scope(|scope| {
            for i in 0..8u8 {
                let path = &path;
                scope.spawn(move || write(path, &[i; 64]).unwrap());
            }
        });
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 64);
        assert!(bytes.iter().all(|b| *b == bytes[0]), "torn write");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(feature = "json")]
    #[test]
    fn json_round_trip() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Config {
            name: String,
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert_eq!(read_json::<Config>(&path).unwrap(), None);
        let value = Config { name: "a".into() };
        write_json(&path, &value).unwrap();
        assert_eq!(read_json::<Config>(&path).unwrap(), Some(value));
        fs::write(&path, b"{").unwrap();
        assert!(matches!(
            read_json::<Config>(&path),
            Err(crate::Error::Json { .. })
        ));
    }
}
