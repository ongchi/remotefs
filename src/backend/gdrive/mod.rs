//! Google Drive backend for `remotefs`.
//!
//! ## Path model
//!
//! Google Drive identifies items by opaque IDs, not paths. This backend
//! maintains a path → ID cache (`PathCache`) that is populated lazily as
//! directories are listed. The FUSE path "/" corresponds to the Drive folder
//! whose ID is `drive_root_id` (defaults to `"root"` = My Drive, or the
//! provided Shared Drive ID).
//!
//! ## Trash
//!
//! A virtual `/.Trash` directory appears at the Drive root listing.
//! Deleting a file moves it to the Drive trash (recoverable). Deleting a file
//! that is already inside `/.Trash` permanently removes it. Renaming a file
//! out of `/.Trash` restores it and moves it to the destination directory.
//!
//! ## Writes
//!
//! Drive has no partial-update API. Every `async_write` reads the full cached
//! file from disk and uploads it. This means write throughput is intentionally
//! lower than SFTP; the trade-off is simplicity and correctness.
//!
//! ## Google-native files
//!
//! Docs / Sheets / Slides are shown as 0-byte regular files. Reading them
//! returns an empty buffer. This is a known limitation.

mod auth;
mod client;

pub use auth::{CredentialsFile, OAuthClient};

/// Authentication result from the pre-fork phase. Carries only pure data
/// (no background threads), so it is safe to pass across `fork()`.
pub enum PreAuth {
    OAuth2(auth::Token),
    ServiceAccount(auth::ServiceAccountKey),
}

use super::{AttrChange, RemoteBackend};
use crate::stat::StatData;
use anyhow::{Context, Result, anyhow};
use client::{DriveFile, DriveHttp};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Trash virtual directory constants
// ---------------------------------------------------------------------------

/// FUSE path of the virtual trash directory.
const TRASH_PATH: &str = "/.Trash";
/// Sentinel Drive "file ID" used in the path cache to represent the trash root.
const TRASH_ID: &str = "__trash__";

// ---------------------------------------------------------------------------
// Path → ID cache
// ---------------------------------------------------------------------------

struct PathCache {
    entries: HashMap<String, String>, // path → file_id
}

impl PathCache {
    fn new(root_id: String) -> Self {
        let mut entries = HashMap::new();
        entries.insert("/".to_string(), root_id);
        Self { entries }
    }

    fn get(&self, path: &str) -> Option<String> {
        self.entries.get(path).cloned()
    }

    fn insert(&mut self, path: String, id: String) {
        self.entries.insert(path, id);
    }

    fn remove_prefix(&mut self, prefix: &str) {
        self.entries
            .retain(|k, _| !k.starts_with(prefix) || k == prefix);
        self.entries.remove(prefix);
    }

    fn rename_prefix(&mut self, old: &str, new: &str) {
        let matching: Vec<(String, String)> = self
            .entries
            .iter()
            .filter(|(k, _)| *k == old || k.starts_with(&format!("{old}/")))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in matching {
            self.entries.remove(&k);
            let new_key = if k == old {
                new.to_string()
            } else {
                format!("{new}{}", &k[old.len()..])
            };
            self.entries.insert(new_key, v);
        }
    }
}

// ---------------------------------------------------------------------------
// Inner shared state
// ---------------------------------------------------------------------------

/// How long to cache the storage-quota response from `about.get`.
const STATFS_TTL: Duration = Duration::from_secs(60);

struct DriveInner {
    http: DriveHttp,
    paths: Mutex<PathCache>,
    /// Cached result of `about.get?fields=storageQuota` to avoid an HTTP round-trip
    /// on every `statfs` call (Finder polls this every few seconds).
    statfs_cache: Mutex<Option<((u64, u64), Instant)>>,
}

// ---------------------------------------------------------------------------
// DriveBackend
// ---------------------------------------------------------------------------

/// Google Drive backend. All clones share the same `DriveInner` (same HTTP
/// client, token, and path cache), so they are cheap to create for the worker
/// pool — no extra network connections are opened.
#[derive(Clone)]
pub struct DriveBackend {
    inner: Arc<DriveInner>,
}

impl DriveBackend {
    /// Phase 1: authenticate only — may open a browser, but creates no
    /// persistent HTTP client. Returns pure data (no background threads),
    /// so it is safe to call before `fork()`.
    pub fn authenticate(creds_path: &Path, email: &str) -> Result<PreAuth> {
        let text = std::fs::read_to_string(creds_path)
            .with_context(|| format!("read credentials {}", creds_path.display()))?;

        if auth::is_service_account(&text) {
            let key: auth::ServiceAccountKey =
                serde_json::from_str(&text).context("parse service account key")?;
            Ok(PreAuth::ServiceAccount(key))
        } else {
            let creds: auth::CredentialsFile =
                serde_json::from_str(&text).context("parse credentials.json")?;
            // Temporary HTTP client — dropped before fork so its tokio runtime
            // is shut down and won't be inherited in a dead state by the child.
            let http = build_http_client();
            let token = auth::load_or_authorize(&creds.installed, &http, email)
                .context("Google Drive authorization")?;
            Ok(PreAuth::OAuth2(token))
        }
    }

    /// Phase 2: connect with a fresh HTTP client.
    ///
    /// Creates a new `reqwest::blocking::Client` (and its tokio runtime) in
    /// the calling process. Always call this **after** `fork()` so the new
    /// runtime is not a dead copy of the parent's.
    pub fn connect_from_auth(
        pre_auth: PreAuth,
        email: &str,
        shared_drive_id: Option<&str>,
    ) -> Result<Arc<Self>> {
        match pre_auth {
            PreAuth::OAuth2(token) => {
                let http = build_http_client();
                let drive_http = DriveHttp::new(http, token, shared_drive_id.map(str::to_string));

                let actual_email = drive_http
                    .account_email()
                    .context("resolve account email")?;
                eprintln!("Authenticated as {actual_email}");

                if actual_email != email {
                    let safe = email.replace(['@', '.'], "_");
                    anyhow::bail!(
                        "expected account {email} but authenticated as {actual_email}\n\
                         Delete ~/.config/remotefs/gdrive_token_{safe}.json to re-authorize"
                    );
                }

                drive_http.set_email(&actual_email);
                Self::finish_connect(drive_http)
            }
            PreAuth::ServiceAccount(key) => {
                let http = build_http_client();
                let token = auth::obtain_service_account_token(&key, &http)
                    .context("obtain service account token")?;
                eprintln!("Authenticated as {} (service account)", key.client_email);
                let drive_http = DriveHttp::new_service_account(
                    http,
                    token,
                    key,
                    shared_drive_id.map(str::to_string),
                );
                Self::finish_connect(drive_http)
            }
        }
    }

    /// Convenience wrapper — authenticate then connect in one call.
    /// Use only when not daemonizing (foreground mode).
    pub fn connect(
        creds_path: &Path,
        email: &str,
        shared_drive_id: Option<&str>,
    ) -> Result<Arc<Self>> {
        let pre_auth = Self::authenticate(creds_path, email)?;
        Self::connect_from_auth(pre_auth, email, shared_drive_id)
    }

    fn finish_connect(drive_http: DriveHttp) -> Result<Arc<Self>> {
        let root_id = resolve_root_id(&drive_http)?;
        log::info!("Connected to Google Drive; root ID = {root_id}");
        let inner = Arc::new(DriveInner {
            http: drive_http,
            paths: Mutex::new(PathCache::new(root_id)),
            statfs_cache: Mutex::new(None),
        });
        Ok(Arc::new(Self { inner }))
    }

    // -----------------------------------------------------------------------
    // Path resolution
    // -----------------------------------------------------------------------

    fn resolve_id(&self, path: &str) -> Result<Option<String>> {
        // Virtual trash directory — always resolves to the sentinel ID.
        if path == TRASH_PATH {
            return Ok(Some(TRASH_ID.to_string()));
        }

        // Fast path: already cached.
        if let Some(id) = self.inner.paths.lock().unwrap().get(path) {
            return Ok(Some(id));
        }

        // Resolve parent, then list its children to find this entry.
        let parent = parent_path(path);
        let parent_id = match self.resolve_id(&parent)? {
            Some(id) => id,
            None => return Ok(None),
        };

        self.populate_children(&parent, &parent_id)?;

        Ok(self.inner.paths.lock().unwrap().get(path))
    }

    /// List `folder_id` and insert all children into the path cache.
    fn populate_children(&self, folder_path: &str, folder_id: &str) -> Result<()> {
        if folder_path == TRASH_PATH {
            self.trash_populate_cache()?;
            return Ok(());
        }

        let children = self
            .inner
            .http
            .list_children(folder_id)
            .with_context(|| format!("list children of {folder_path}"))?;

        let mut cache = self.inner.paths.lock().unwrap();
        for f in &children {
            let child = child_path(folder_path, &f.name);
            cache.insert(child, f.id.clone());
        }
        Ok(())
    }

    fn resolve_file(&self, path: &str) -> Result<Option<DriveFile>> {
        match self.resolve_id(path)? {
            Some(id) => {
                let f = self
                    .inner
                    .http
                    .get_file(&id)
                    .with_context(|| format!("get_file {path}"))?;
                Ok(Some(f))
            }
            None => Ok(None),
        }
    }

    fn resolve_parent(&self, path: &str) -> Result<(String, String)> {
        let parent = parent_path(path);
        let name = base_name(path).to_string();
        let parent_id = self
            .resolve_id(&parent)?
            .ok_or_else(|| anyhow!("parent not found: {parent}"))?;
        Ok((parent_id, name))
    }

    // -----------------------------------------------------------------------
    // Trash helpers
    // -----------------------------------------------------------------------

    /// List all trashed files, populate the path cache under `/.Trash/`, and
    /// return `(display_name, stat)` pairs.
    ///
    /// When two trashed files share the same filename, duplicates are
    /// disambiguated by appending `_1`, `_2`, … to all but the first.
    fn trash_populate_cache(&self) -> Result<Vec<(String, StatData)>> {
        let files = self.inner.http.list_trashed()?;

        // Count occurrences of each name to detect duplicates.
        let mut name_counts: HashMap<String, u32> = HashMap::new();
        for f in &files {
            *name_counts.entry(f.name.clone()).or_insert(0) += 1;
        }

        let mut name_seq: HashMap<String, u32> = HashMap::new();
        let mut entries: Vec<(String, String)> = Vec::with_capacity(files.len());
        let mut result: Vec<(String, StatData)> = Vec::with_capacity(files.len());

        for f in &files {
            let total = name_counts[&f.name];
            let display = if total == 1 {
                f.name.clone()
            } else {
                let seq = name_seq.entry(f.name.clone()).or_insert(0);
                *seq += 1;
                format!("{}_{}", f.name, seq)
            };
            entries.push((format!("{TRASH_PATH}/{display}"), f.id.clone()));
            result.push((display, drive_file_to_stat(f)));
        }

        let mut cache = self.inner.paths.lock().unwrap();
        for (path, id) in entries {
            cache.insert(path, id);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// RemoteBackend impl
// ---------------------------------------------------------------------------

impl RemoteBackend for DriveBackend {
    fn stat(&self, path: &str) -> Option<StatData> {
        // Virtual trash directory
        if path == TRASH_PATH {
            return Some(trash_dir_stat());
        }

        if path == "/" {
            // Return a synthetic stat for the mount root — avoids an API call
            // on every kernel revalidation while still reporting a valid directory.
            return Some(root_dir_stat());
        }

        match self.resolve_file(path) {
            Ok(Some(f)) => Some(drive_file_to_stat(&f)),
            Ok(None) => None,
            Err(e) => {
                log::error!("stat {path}: {e}");
                None
            }
        }
    }

    fn readdir(&self, path: &str) -> Result<Vec<(String, StatData)>> {
        // Virtual trash listing
        if path == TRASH_PATH {
            return self.trash_populate_cache();
        }

        let folder_id = self
            .resolve_id(path)?
            .ok_or_else(|| anyhow!("readdir: {path} not found"))?;

        let children = self
            .inner
            .http
            .list_children(&folder_id)
            .with_context(|| format!("readdir {path}"))?;

        {
            let mut cache = self.inner.paths.lock().unwrap();
            for f in &children {
                cache.insert(child_path(path, &f.name), f.id.clone());
            }
        }

        let mut result: Vec<(String, StatData)> = children
            .into_iter()
            .map(|f| {
                let stat = drive_file_to_stat(&f);
                (f.name, stat)
            })
            .collect();

        // Inject the virtual .Trash entry at the Drive root.
        if path == "/" {
            result.push((".Trash".to_string(), trash_dir_stat()));
        }

        Ok(result)
    }

    fn download_to(&self, path: &str, dest: &Path, max_bytes: u64) -> bool {
        let id = match self.resolve_id(path) {
            Ok(Some(id)) => id,
            Ok(None) => {
                log::error!("download_to: {path} not found");
                return false;
            }
            Err(e) => {
                log::error!("download_to resolve {path}: {e}");
                return false;
            }
        };

        let file = match self.inner.http.get_file(&id) {
            Ok(f) => f,
            Err(e) => {
                log::error!("download_to get_file {path}: {e}");
                return false;
            }
        };
        if file.size_bytes() > max_bytes {
            return false;
        }

        log::info!("download_to {path}");
        let tmp = PathBuf::from(format!("{}.tmp", dest.display()));

        let result: Result<()> = (|| {
            let content = self.inner.http.download(&id)?;
            std::fs::write(&tmp, &content)?;
            std::fs::rename(&tmp, dest)?;
            Ok(())
        })();

        match result {
            Ok(_) => true,
            Err(e) => {
                log::error!("download_to {path}: {e}");
                let _ = std::fs::remove_file(&tmp);
                false
            }
        }
    }

    fn read_at(&self, path: &str, offset: u64, size: usize) -> Result<Vec<u8>> {
        let id = self
            .resolve_id(path)?
            .ok_or_else(|| anyhow!("read_at: {path} not found"))?;
        let content = self.inner.http.download(&id)?;
        let start = (offset as usize).min(content.len());
        let end = (start + size).min(content.len());
        Ok(content[start..end].to_vec())
    }

    fn async_write(
        &self,
        path: &str,
        _offset: u64,
        _data: &[u8],
        local_cache: &Path,
    ) -> Result<()> {
        let id = self
            .resolve_id(path)?
            .ok_or_else(|| anyhow!("async_write: {path} not found"))?;
        let content =
            std::fs::read(local_cache).with_context(|| format!("read local cache for {path}"))?;
        self.inner.http.upload(&id, content)?;
        Ok(())
    }

    fn create_file(&self, path: &str, _mode: u32) -> Result<()> {
        let (parent_id, name) = self.resolve_parent(path)?;
        let f = self.inner.http.create_file(&parent_id, &name)?;
        self.inner
            .paths
            .lock()
            .unwrap()
            .insert(path.to_string(), f.id);
        Ok(())
    }

    fn create_dir(&self, path: &str, _mode: u32) -> Result<()> {
        let (parent_id, name) = self.resolve_parent(path)?;
        let f = self.inner.http.create_folder(&parent_id, &name)?;
        self.inner
            .paths
            .lock()
            .unwrap()
            .insert(path.to_string(), f.id);
        Ok(())
    }

    fn delete_file(&self, path: &str) -> Result<()> {
        let id = self
            .resolve_id(path)?
            .ok_or_else(|| anyhow!("delete_file: {path} not found"))?;

        if is_in_trash(path) {
            // Already in trash — permanently delete.
            self.inner.http.delete_permanent(&id)?;
        } else {
            // Move to trash (recoverable).
            self.inner.http.trash_file(&id)?;
        }
        self.inner.paths.lock().unwrap().remove_prefix(path);
        Ok(())
    }

    fn delete_dir(&self, path: &str) -> Result<()> {
        let id = self
            .resolve_id(path)?
            .ok_or_else(|| anyhow!("delete_dir: {path} not found"))?;

        if is_in_trash(path) {
            self.inner.http.delete_permanent(&id)?;
        } else {
            self.inner.http.trash_file(&id)?;
        }
        self.inner.paths.lock().unwrap().remove_prefix(path);
        Ok(())
    }

    fn rename(&self, old: &str, new: &str) -> Result<()> {
        // Restore from trash: untrash + move to the destination in one call.
        if is_in_trash(old) {
            let id = self
                .resolve_id(old)?
                .ok_or_else(|| anyhow!("rename: {old} not found in trash"))?;
            let (new_parent_id, new_name) = self.resolve_parent(new)?;
            self.inner
                .http
                .restore_file(&id, &new_parent_id, &new_name)?;
            let mut cache = self.inner.paths.lock().unwrap();
            cache.remove_prefix(old);
            cache.insert(new.to_string(), id);
            return Ok(());
        }

        let id = self
            .resolve_id(old)?
            .ok_or_else(|| anyhow!("rename source not found: {old}"))?;
        let old_parent_id = self
            .resolve_id(&parent_path(old))?
            .ok_or_else(|| anyhow!("old parent not found: {}", parent_path(old)))?;
        let (new_parent_id, new_name) = self.resolve_parent(new)?;

        self.inner
            .http
            .rename(&id, &new_name, &old_parent_id, &new_parent_id)?;

        self.inner.paths.lock().unwrap().rename_prefix(old, new);
        Ok(())
    }

    fn setstat(&self, path: &str, _change: AttrChange) -> Result<()> {
        log::debug!("setstat {path}: ignored (Drive does not support POSIX attrs)");
        Ok(())
    }

    fn symlink(&self, _target: &str, _linkname: &str) -> Result<()> {
        Err(anyhow!("symlinks are not supported on Google Drive"))
    }

    fn readlink(&self, path: &str) -> Result<String> {
        Err(anyhow!("readlink not supported on Google Drive: {path}"))
    }

    fn statfs(&self) -> Option<(u64, u64)> {
        let mut cache = self.inner.statfs_cache.lock().unwrap();
        if let Some((result, fetched_at)) = *cache {
            if fetched_at.elapsed() < STATFS_TTL {
                return Some(result);
            }
        }
        let result = self.inner.http.storage_quota().ok()?;
        *cache = Some((result, Instant::now()));
        Some(result)
    }

    fn new_worker(&self) -> Result<Arc<dyn RemoteBackend>> {
        Ok(Arc::new(self.clone()))
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn drive_file_to_stat(f: &DriveFile) -> StatData {
    let perm = if f.is_dir() { 0o040_755 } else { 0o100_644 };
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mtime = f.mtime_secs();
    StatData {
        size: f.size_bytes(),
        uid,
        gid,
        perm,
        atime: mtime,
        mtime,
    }
}

fn root_dir_stat() -> StatData {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    StatData {
        size: 0,
        uid,
        gid,
        perm: 0o040_755,
        atime: 0,
        mtime: 0,
    }
}

fn trash_dir_stat() -> StatData {
    root_dir_stat()
}

// ---------------------------------------------------------------------------
// Path utilities
// ---------------------------------------------------------------------------

fn parent_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => path[..idx].to_string(),
    }
}

fn base_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Returns `true` when `path` is a direct child of `/.Trash`
/// (i.e. the path is `/.Trash/<name>`, not `/.Trash` itself).
fn is_in_trash(path: &str) -> bool {
    path.starts_with(TRASH_PATH) && path.len() > TRASH_PATH.len()
}

// ---------------------------------------------------------------------------
// Module-level helpers
// ---------------------------------------------------------------------------

fn build_http_client() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()
}

fn resolve_root_id(drive_http: &DriveHttp) -> Result<String> {
    if let Some(drive_id) = drive_http.shared_drive() {
        // Drive's Shared Drive root folder ID equals the drive ID itself.
        log::info!("Using Shared Drive ID = {drive_id} as mount root");
        Ok(drive_id.to_string())
    } else {
        let root = drive_http
            .get_file("root")
            .context("resolve My Drive root")?;
        Ok(root.id)
    }
}
