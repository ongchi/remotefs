//! Hash-striped per-path mutex.
//!
//! callers receive a `PathGuard` whose drop releases the stripe lock.

use std::sync::{Mutex, MutexGuard};
use xxhash_rust::xxh64::xxh64;

const NUM_STRIPES: usize = 2048;

pub struct StripedLock {
    locks: Vec<Mutex<()>>,
}

/// Holds one stripe's lock; released on drop.
pub struct PathGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

impl Default for StripedLock {
    fn default() -> Self {
        Self::new()
    }
}

impl StripedLock {
    pub fn new() -> Self {
        let locks = (0..NUM_STRIPES).map(|_| Mutex::new(())).collect();
        Self { locks }
    }

    fn stripe(&self, path: &str) -> usize {
        xxh64(path.as_bytes(), 0) as usize % self.locks.len()
    }

    /// Blocks until the stripe for `path` is locked.
    pub fn lock(&self, path: &str) -> PathGuard<'_> {
        PathGuard {
            _guard: self.locks[self.stripe(path)].lock().unwrap(),
        }
    }

    /// Non-blocking attempt; returns `None` if stripe is already held.
    pub fn try_lock(&self, path: &str) -> Option<PathGuard<'_>> {
        self.locks[self.stripe(path)]
            .try_lock()
            .ok()
            .map(|g| PathGuard { _guard: g })
    }

    /// Returns `true` if the stripe for `path` is currently held.
    pub fn is_locked(&self, path: &str) -> bool {
        self.locks[self.stripe(path)].try_lock().is_err()
    }
}
