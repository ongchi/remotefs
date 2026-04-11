//! Trait for remote filesystem backends (SFTP, Google Drive, …).

pub mod gdrive;
pub mod sftp;

use crate::stat::StatData;
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;

/// Attribute changes to apply to a remote path.
pub struct AttrChange {
    pub size: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub perm: Option<u32>,
    pub atime: Option<u64>,
    pub mtime: Option<u64>,
}

/// Abstraction over a remote filesystem.
///
/// All methods use `&self`; implementations supply interior mutability (Mutex,
/// RwLock, Arc) where needed. The type must be `Send + Sync + 'static` so it
/// can be shared across FUSE callbacks and rayon worker threads.
pub trait RemoteBackend: Send + Sync + 'static {
    /// Stat a path. Returns `None` if the path does not exist (ENOENT).
    fn stat(&self, path: &str) -> Option<StatData>;

    /// List a directory. Returns `(child_name, stat)` pairs.
    fn readdir(&self, path: &str) -> Result<Vec<(String, StatData)>>;

    /// Download the full file at `path` to `dest` atomically (via temp file).
    ///
    /// Returns `false` and leaves `dest` untouched if the file is larger than
    /// `max_bytes` or the download fails.
    fn download_to(&self, path: &str, dest: &Path, max_bytes: u64) -> bool;

    /// Read a slice of a remote file directly (cache-miss fallback, no caching).
    fn read_at(&self, path: &str, offset: u64, size: usize) -> Result<Vec<u8>>;

    /// Persist a write to the remote asynchronously.
    ///
    /// `offset`+`data` describe the changed chunk; `local_cache` is the full
    /// cached file on disk. Implementations choose the appropriate strategy:
    ///
    /// - **SFTP**: seeks to `offset` and writes `data` in-place (fast path).
    /// - **Drive**: reads `local_cache` and uploads the whole file (Drive has
    ///   no partial-update API).
    fn async_write(&self, path: &str, offset: u64, data: &[u8], local_cache: &Path)
        -> Result<()>;

    fn create_file(&self, path: &str, mode: u32) -> Result<()>;
    fn create_dir(&self, path: &str, mode: u32) -> Result<()>;
    fn delete_file(&self, path: &str) -> Result<()>;
    fn delete_dir(&self, path: &str) -> Result<()>;
    fn rename(&self, old: &str, new: &str) -> Result<()>;
    fn setstat(&self, path: &str, change: AttrChange) -> Result<()>;
    fn symlink(&self, target: &str, linkname: &str) -> Result<()>;
    fn readlink(&self, path: &str) -> Result<String>;

    /// Return `(total_bytes, free_bytes)` for the remote filesystem.
    ///
    /// Used to populate `statfs` so Finder can display accurate disk usage.
    /// Returns `None` if the backend cannot determine capacity.
    fn statfs(&self) -> Option<(u64, u64)> {
        None
    }

    /// Create a backend instance for use in a worker thread.
    ///
    /// - **SFTP**: opens a fresh TCP + SSH connection.
    /// - **Drive**: returns a cheap `Arc::clone` of the shared HTTP client.
    fn new_worker(&self) -> Result<Arc<dyn RemoteBackend>>;
}
