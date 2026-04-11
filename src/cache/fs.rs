//! Disk-backed LRU cache for remote file contents.
//!
//! Cache files are named by the xxh64 hex digest of the remote path.
//! Eviction is by total byte size (default 1 GiB).

use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use xxhash_rust::xxh64::xxh64;

pub struct CacheManager {
    pub cache_path: PathBuf,
    /// Maximum total cache size in bytes.
    pub max_size: u64,
    inner: Mutex<Inner>,
}

struct Inner {
    total_size: u64,
    /// Insertion-ordered map used as LRU: oldest at front, newest at back.
    cache: IndexMap<PathBuf, u64>,
}

impl CacheManager {
    pub fn new(cache_path: PathBuf, max_disk_mb: u64) -> anyhow::Result<Self> {
        let max_size = max_disk_mb << 20;
        let mut inner = Inner {
            total_size: 0,
            cache: IndexMap::new(),
        };

        if !cache_path.exists() {
            std::fs::create_dir_all(&cache_path)?;
        } else {
            for entry in std::fs::read_dir(&cache_path)? {
                let entry = entry?;
                let path = entry.path();
                // Skip metadata files (meta_cache.json, dir_cache.json, *.tmp).
                // Content cache files are named by hex hashes and have no extension.
                if path.extension().is_some() {
                    continue;
                }
                let size = entry.metadata()?.len();
                inner.total_size += size;
                inner.cache.insert(path, size);
            }
        }

        Ok(Self {
            cache_path,
            max_size,
            inner: Mutex::new(inner),
        })
    }

    /// Returns the local cache file path for the given remote path.
    pub fn cachefile(&self, remote_path: &str) -> PathBuf {
        let digest = xxh64(remote_path.as_bytes(), 0);
        self.cache_path.join(format!("{:016x}", digest))
    }

    /// Marks a cache file as recently used (move to end of LRU).
    pub fn renew(&self, key: &Path) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&size) = inner.cache.get(key) {
            inner.cache.shift_remove(key);
            inner.cache.insert(key.to_path_buf(), size);
        }
    }

    /// Records that a cache file was written/updated; evicts oldest entries
    /// if the total size exceeds the limit.
    pub fn put(&self, key: &Path) {
        let new_size = match key.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        let mut to_delete = vec![];
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(&old) = inner.cache.get(key) {
                inner.total_size = inner.total_size.saturating_sub(old);
            }
            inner.cache.shift_remove(key);
            inner.total_size += new_size;
            inner.cache.insert(key.to_path_buf(), new_size);

            while inner.total_size > self.max_size && !inner.cache.is_empty() {
                // `shift_remove_index(0)` removes the oldest (front) entry.
                if let Some((k, s)) = inner.cache.shift_remove_index(0) {
                    inner.total_size = inner.total_size.saturating_sub(s);
                    to_delete.push(k);
                }
            }
        }
        for f in to_delete {
            let _ = std::fs::remove_file(f);
        }
    }

    /// Removes a cache file from tracking and deletes it from disk.
    pub fn pop(&self, key: &Path) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(&s) = inner.cache.get(key) {
                inner.total_size = inner.total_size.saturating_sub(s);
            }
            inner.cache.shift_remove(key);
        }
        let _ = std::fs::remove_file(key);
    }
}
