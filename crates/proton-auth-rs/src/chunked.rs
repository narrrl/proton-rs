use crate::{Result, SecretStore};

/// The most UTF-16 code units one entry of the underlying store is given.
///
/// Windows caps a credential's blob at 2560 bytes and stores a password as
/// UTF-16: 1280 units. A signed-in session (tokens, scopes and a salted key
/// passphrase per address) can pass that, and failing to save a refreshed
/// token signs the user out at the next start. The margin is for the header.
pub const ENTRY_UNITS: usize = 1200;

/// Marks a split value. Kept from Proton Stream, which wrote it first, so its
/// stored secrets still read.
pub const CHUNK_HEADER: &str = "pstr-chunked:";

/// A [`SecretStore`] that splits a value too long for one entry of `S`.
///
/// A short value is one entry, exactly as `S` would store it, so secrets
/// saved before splitting existed still read. A long one is written as
/// `key#0`, `key#1`, … with `key` itself holding [`CHUNK_HEADER`] and the
/// count. The parts go first and the header last, so a reader never finds a
/// header naming parts that are not there yet.
#[derive(Debug, Default)]
pub struct Chunked<S>(pub S);

impl<S> Chunked<S> {
    pub fn new(store: S) -> Self {
        Self(store)
    }

    pub fn inner(&self) -> &S {
        &self.0
    }
}

fn part(key: &str, index: usize) -> String {
    format!("{key}#{index}")
}

fn parts_of(header: &str) -> Option<usize> {
    header.strip_prefix(CHUNK_HEADER)?.parse().ok()
}

/// `value` cut into pieces of at most [`ENTRY_UNITS`] UTF-16 units, never
/// inside a character.
fn split(value: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut units = 0;
    for (at, ch) in value.char_indices() {
        if units + ch.len_utf16() > ENTRY_UNITS {
            pieces.push(&value[start..at]);
            start = at;
            units = 0;
        }
        units += ch.len_utf16();
    }
    pieces.push(&value[start..]);
    pieces
}

impl<S: SecretStore> SecretStore for Chunked<S> {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let store = &self.0;
        let before = store.get(key)?.as_deref().and_then(parts_of).unwrap_or(0);
        // A short value that happens to start with the header is split too,
        // so it cannot be mistaken for one.
        let fits = value.encode_utf16().count() <= ENTRY_UNITS && !value.starts_with(CHUNK_HEADER);
        let written = if fits {
            store.set(key, value)?;
            0
        } else {
            let pieces = split(value);
            for (index, piece) in pieces.iter().enumerate() {
                store.set(&part(key, index), piece)?;
            }
            store.set(key, &format!("{CHUNK_HEADER}{}", pieces.len()))?;
            pieces.len()
        };
        for index in written..before {
            store.delete(&part(key, index))?;
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        let store = &self.0;
        let Some(value) = store.get(key)? else {
            return Ok(None);
        };
        let Some(count) = parts_of(&value) else {
            return Ok(Some(value));
        };
        let mut joined = String::new();
        for index in 0..count {
            // A part gone missing is a value that no longer exists whole;
            // half of a token is worse than none.
            let Some(piece) = store.get(&part(key, index))? else {
                return Ok(None);
            };
            joined.push_str(&piece);
        }
        Ok(Some(joined))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let store = &self.0;
        let count = store.get(key)?.as_deref().and_then(parts_of).unwrap_or(0);
        store.delete(key)?;
        for index in 0..count {
            store.delete(&part(key, index))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    /// An underlying store that refuses what Windows would.
    #[derive(Default)]
    struct Limited(MemoryStore);

    impl SecretStore for Limited {
        fn set(&self, key: &str, value: &str) -> Result<()> {
            assert!(
                value.encode_utf16().count() <= 1280,
                "{key} is too long for one entry"
            );
            self.0.set(key, value)
        }
        fn get(&self, key: &str) -> Result<Option<String>> {
            self.0.get(key)
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.0.delete(key)
        }
    }

    fn store() -> Chunked<Limited> {
        Chunked::new(Limited::default())
    }

    #[test]
    fn a_short_secret_is_one_entry_as_it_always_was() {
        let store = store();
        store.set("share-a", "fragment").unwrap();
        assert_eq!(store.inner().0.len(), 1);
        assert_eq!(
            store.inner().get("share-a").unwrap().as_deref(),
            Some("fragment")
        );
        assert_eq!(store.get("share-a").unwrap().as_deref(), Some("fragment"));
    }

    #[test]
    fn a_secret_longer_than_an_entry_round_trips() {
        let store = store();
        let long: String = "aé😀".repeat(1500);
        store.set("proton-account", &long).unwrap();
        assert!(store.inner().0.len() > 2);
        assert_eq!(store.get("proton-account").unwrap(), Some(long));
    }

    #[test]
    fn a_shorter_value_leaves_no_parts_of_the_longer_one_behind() {
        let store = store();
        store.set("k", &"x".repeat(5000)).unwrap();
        store.set("k", &"y".repeat(1300)).unwrap();
        assert_eq!(store.inner().0.len(), 3);
        store.set("k", "short").unwrap();
        assert_eq!(store.inner().0.len(), 1);
        assert_eq!(store.get("k").unwrap().as_deref(), Some("short"));
    }

    #[test]
    fn a_value_that_looks_like_a_header_is_not_taken_for_one() {
        let store = store();
        store.set("k", "pstr-chunked:3").unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("pstr-chunked:3"));
    }

    #[test]
    fn deleting_a_split_secret_deletes_every_part() {
        let store = store();
        store.set("k", &"x".repeat(5000)).unwrap();
        store.delete("k").unwrap();
        assert!(store.inner().0.is_empty());
        assert_eq!(store.get("k").unwrap(), None);
    }

    #[test]
    fn a_split_secret_missing_a_part_reads_as_absent() {
        let store = store();
        store.set("k", &"x".repeat(5000)).unwrap();
        store.inner().delete("k#1").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
    }
}
