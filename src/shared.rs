//! Shared state accessed by the FUSE loop, task pool, and cache updater.

use crate::backend::RemoteBackend;
use crate::cache::fs::CacheManager;
use crate::cache::meta::MetaCache;
use crate::lock::StripedLock;
use crate::stat::StatData;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Persisted metadata older than this is discarded on load.
const PERSIST_MAX_AGE_SECS: u64 = 600;

#[derive(Serialize, Deserialize)]
struct PersistedMetaCache {
    saved_at: u64,
    entries: HashMap<String, Option<StatData>>,
}

#[derive(Serialize, Deserialize)]
struct PersistedDirCache {
    saved_at: u64,
    entries: HashMap<String, Vec<String>>,
}

/// Attr cache value: `None` = path confirmed absent (ENOENT).
pub type AttrCache = MetaCache<String, Option<StatData>>;
pub type DirCache = MetaCache<String, Vec<String>>;

pub struct Shared {
    /// Backend used as a factory for worker-thread connections.
    pub backend: Arc<dyn RemoteBackend>,
    pub attr_cache: AttrCache,
    pub dir_cache: DirCache,
    pub disk_cache: CacheManager,
    pub path_lock: StripedLock,
    /// Paths whose directory listing is being fetched in the background.
    /// Prevents duplicate concurrent fetches for the same directory.
    pub fetching_dirs: Mutex<HashSet<String>>,
}

impl Shared {
    pub fn new(
        backend: Arc<dyn RemoteBackend>,
        cache_path: PathBuf,
        max_disk_mb: u64,
    ) -> anyhow::Result<Self> {
        let shared = Self {
            backend,
            attr_cache: MetaCache::new(1 << 18),
            dir_cache: MetaCache::new(1 << 18),
            disk_cache: CacheManager::new(cache_path, max_disk_mb)?,
            path_lock: StripedLock::new(),
            fetching_dirs: Mutex::new(HashSet::new()),
        };
        shared.load_from_disk();
        Ok(shared)
    }

    /// Serialize both metadata caches to disk.  Called on unmount so the
    /// next mount starts with a warm cache instead of making fresh API calls
    /// for every directory listing and file attribute.
    pub fn save(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let meta_path = self.disk_cache.cache_path.join("meta_cache.json");
        let entries: HashMap<String, Option<StatData>> =
            self.attr_cache.snapshot().into_iter().collect();
        let count = entries.len();
        write_json(&meta_path, &PersistedMetaCache { saved_at: now, entries });
        log::info!("saved {count} attr cache entries to {}", meta_path.display());

        let dir_path = self.disk_cache.cache_path.join("dir_cache.json");
        let entries: HashMap<String, Vec<String>> =
            self.dir_cache.snapshot().into_iter().collect();
        let count = entries.len();
        write_json(&dir_path, &PersistedDirCache { saved_at: now, entries });
        log::info!("saved {count} dir cache entries to {}", dir_path.display());
    }

    /// Load metadata caches from disk.  Silently skips stale or missing files.
    fn load_from_disk(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let meta_path = self.disk_cache.cache_path.join("meta_cache.json");
        match read_json::<PersistedMetaCache>(&meta_path) {
            Ok(p) if now.saturating_sub(p.saved_at) <= PERSIST_MAX_AGE_SECS => {
                let count = p.entries.len();
                for (k, v) in p.entries {
                    self.attr_cache.put(k, v);
                }
                log::info!("loaded {count} persisted attr entries");
            }
            Ok(_) => log::info!("persisted meta_cache is stale, skipping"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("load meta_cache: {e}"),
        }

        let dir_path = self.disk_cache.cache_path.join("dir_cache.json");
        match read_json::<PersistedDirCache>(&dir_path) {
            Ok(p) if now.saturating_sub(p.saved_at) <= PERSIST_MAX_AGE_SECS => {
                let count = p.entries.len();
                for (k, v) in p.entries {
                    self.dir_cache.put(k, v);
                }
                log::info!("loaded {count} persisted dir entries");
            }
            Ok(_) => log::info!("persisted dir_cache is stale, skipping"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("load dir_cache: {e}"),
        }
    }
}

fn write_json<T: Serialize>(path: &std::path::Path, value: &T) {
    let json = match serde_json::to_string(value) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("serialize {}: {e}", path.display());
            return;
        }
    };
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, &json) {
        log::warn!("write {}: {e}", tmp.display());
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        log::warn!("rename {} -> {}: {e}", tmp.display(), path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &std::path::Path) -> std::io::Result<T> {
    let data = std::fs::read(path)?;
    serde_json::from_slice(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
