//! Bounded, expiring in-memory key/value store.
//!
//! Backs every short-lived, client-fed map in the gateway (pending enrollment
//! codes, OAuth authorization codes, dynamically registered OAuth clients).
//! Entries expire after a fixed TTL and the map has a hard size cap, so an
//! unauthenticated caller cannot grow it without bound. The lock is
//! `parking_lot`'s, which has no poisoning: a panic elsewhere cannot turn into
//! a panic on every later request.

use parking_lot::{Mutex, MutexGuard};
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Entry<V> {
    value: V,
    inserted: Instant,
}

/// The store is at its size cap and nothing has expired.
#[derive(Debug, PartialEq, Eq)]
pub struct Full;

pub struct ExpiringStore<V> {
    entries: Mutex<HashMap<String, Entry<V>>>,
    ttl: Duration,
    max_entries: usize,
}

impl<V> ExpiringStore<V> {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry<V>>> {
        self.entries.lock()
    }

    fn reap(&self, map: &mut HashMap<String, Entry<V>>) {
        let ttl = self.ttl;
        map.retain(|_, e| e.inserted.elapsed() < ttl);
    }

    /// Insert `value`, refusing when the store is full of live entries.
    pub fn insert(&self, key: String, value: V) -> Result<(), Full> {
        let mut map = self.lock();
        self.reap(&mut map);
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            return Err(Full);
        }
        map.insert(
            key,
            Entry {
                value,
                inserted: Instant::now(),
            },
        );
        Ok(())
    }

    /// Reserve a slot, then build the value under the same lock. Refuses with
    /// [`Full`] *before* calling `make`, so a side effect in `make` (consuming a
    /// single-use code elsewhere) never happens for a refused insert. Returns
    /// `Ok(false)` when `make` yields nothing and so nothing was inserted.
    /// `make` must not touch this store.
    pub fn insert_with(&self, key: String, make: impl FnOnce() -> Option<V>) -> Result<bool, Full> {
        let mut map = self.lock();
        self.reap(&mut map);
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            return Err(Full);
        }
        let Some(value) = make() else {
            return Ok(false);
        };
        map.insert(
            key,
            Entry {
                value,
                inserted: Instant::now(),
            },
        );
        Ok(true)
    }

    /// Insert `value`, evicting the oldest live entry when the store is full.
    pub fn insert_evicting_oldest(&self, key: String, value: V) {
        let mut map = self.lock();
        self.reap(&mut map);
        if map.len() >= self.max_entries && !map.contains_key(&key) {
            let oldest = map
                .iter()
                .min_by_key(|(_, e)| e.inserted)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                map.remove(&oldest);
            }
        }
        map.insert(
            key,
            Entry {
                value,
                inserted: Instant::now(),
            },
        );
    }

    /// Remove and return the live entry for `key` (single use).
    pub fn take(&self, key: &str) -> Option<V> {
        let entry = self.lock().remove(key)?;
        (entry.inserted.elapsed() < self.ttl).then_some(entry.value)
    }

    /// Number of entries currently held (live or not yet reaped).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }
}

impl<V: Clone> ExpiringStore<V> {
    /// Clone the live entry for `key` without consuming it.
    pub fn get(&self, key: &str) -> Option<V> {
        let map = self.lock();
        let entry = map.get(key)?;
        (entry.inserted.elapsed() < self.ttl).then(|| entry.value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn take_is_single_use() {
        let store = ExpiringStore::new(LONG, 4);
        store.insert("k".into(), 1).unwrap();
        assert_eq!(store.take("k"), Some(1));
        assert_eq!(store.take("k"), None);
    }

    #[test]
    fn expired_entries_are_not_returned() {
        let store = ExpiringStore::new(Duration::ZERO, 4);
        store.insert("k".into(), 1).unwrap();
        assert_eq!(store.get("k"), None);
        assert_eq!(store.take("k"), None);
    }

    #[test]
    fn insert_is_refused_at_the_cap_but_expired_entries_make_room() {
        let store = ExpiringStore::new(LONG, 2);
        store.insert("a".into(), 1).unwrap();
        store.insert("b".into(), 2).unwrap();
        assert_eq!(store.insert("c".into(), 3), Err(Full));
        assert_eq!(store.len(), 2);
        // Re-inserting an existing key is not growth.
        store.insert("a".into(), 10).unwrap();

        let expiring = ExpiringStore::new(Duration::ZERO, 2);
        expiring.insert("a".into(), 1).unwrap();
        expiring.insert("b".into(), 2).unwrap();
        expiring.insert("c".into(), 3).unwrap();
        assert!(expiring.len() <= 2);
    }

    #[test]
    fn evicting_insert_drops_the_oldest() {
        let store = ExpiringStore::new(LONG, 2);
        store.insert_evicting_oldest("a".into(), 1);
        store.insert_evicting_oldest("b".into(), 2);
        store.insert_evicting_oldest("c".into(), 3);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get("a"), None);
        assert_eq!(store.get("b"), Some(2));
        assert_eq!(store.get("c"), Some(3));
    }

    #[test]
    fn a_panic_while_holding_the_lock_does_not_break_later_callers() {
        let store = std::sync::Arc::new(ExpiringStore::new(LONG, 4));
        store.insert("k".into(), 1).unwrap();
        let panicker = store.clone();
        let joined = std::thread::spawn(move || {
            let _guard = panicker.entries.lock();
            panic!("panic while the lock is held");
        })
        .join();
        assert!(joined.is_err());
        assert_eq!(store.get("k"), Some(1));
        store.insert("j".into(), 2).unwrap();
        assert_eq!(store.take("j"), Some(2));
    }
}
