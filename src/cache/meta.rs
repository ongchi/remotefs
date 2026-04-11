//! In-memory LRU cache for file attributes and directory listings.

use lru::LruCache;
use std::borrow::Borrow;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::Mutex;

pub struct MetaCache<K, V> {
    inner: Mutex<LruCache<K, V>>,
}

impl<K: Eq + Hash + Clone, V: Clone> MetaCache<K, V> {
    pub fn new(maxsize: usize) -> Self {
        let cap = NonZeroUsize::new(maxsize).expect("maxsize must be > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Returns the value and promotes it to most-recently-used.
    pub fn get<Q>(&self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut cache = self.inner.lock().unwrap();
        cache.get(k).cloned()
    }

    pub fn put(&self, k: K, v: V) {
        let mut cache = self.inner.lock().unwrap();
        cache.put(k, v);
    }

    pub fn remove<Q>(&self, k: &Q)
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut cache = self.inner.lock().unwrap();
        cache.pop(k);
    }

    /// Snapshot of all entries at this instant (for background refresh).
    pub fn snapshot(&self) -> Vec<(K, V)> {
        let cache = self.inner.lock().unwrap();
        cache.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}
