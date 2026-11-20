use std::collections::BTreeMap;
use std::sync::Mutex;

use zeroize::Zeroizing;

use crate::{Result, SecretStore};

/// Secrets kept in this process's memory only, wiped when dropped.
///
/// For tests, and for a session the user chose not to remember.
#[derive(Debug, Default)]
pub struct MemoryStore(Mutex<BTreeMap<String, Zeroizing<String>>>);

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many keys are stored.
    pub fn len(&self) -> usize {
        self.entries().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Zeroizing<String>>> {
        // A panic while holding the lock leaves the map itself intact.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretStore for MemoryStore {
    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.entries()
            .insert(key.to_owned(), Zeroizing::new(value.to_owned()));
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.entries().get(key).map(|value| value.to_string()))
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.entries().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_secret_reads_back_until_deleted() {
        let store = MemoryStore::new();
        assert_eq!(store.get("k").unwrap(), None);
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("v"));
        store.delete("k").unwrap();
        store.delete("k").unwrap();
        assert_eq!(store.get("k").unwrap(), None);
    }
}
