pub mod backend;
pub mod cache;
pub mod lock;
pub mod mount;
pub mod shared;
pub mod stat;
pub mod updater;

// ---------------------------------------------------------------------------
// FUSE filesystem implementation (RemoteFs + InodeTable)
//
// Key design decisions:
// - fusepy is path-based; fuser is inode-based.  An `InodeTable` maps
//   inode numbers to full remote paths.
// - The fuser single-threaded loop serialises all FS calls so the primary
//   `backend` reference needs no extra lock beyond what the backend itself
//   provides.
// - Async writes go to a rayon thread pool; each pool thread has its own
//   worker backend via `backend.new_worker()` (thread-local storage).
// ---------------------------------------------------------------------------

use crate::backend::{AttrChange, RemoteBackend};
use crate::shared::Shared;
use crate::stat::{StatData, to_file_attr};
use crate::updater::CacheUpdater;
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, Notifier, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyIoctl, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};
use rayon::ThreadPool;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Attribute/entry TTL handed to the kernel.
///
/// Controls how long the kernel caches dentry and inode data before calling
/// back into the FUSE daemon to revalidate. 1 s is appropriate for local
/// filesystems; for a remote with ~200 ms API latency every revalidation is
/// expensive. 10 s cuts kernel round-trips by 10× while still reflecting
/// remote changes within a reasonable window.
const TTL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Inode table
// ---------------------------------------------------------------------------

struct InodeTable {
    ino_to_path: HashMap<u64, String>,
    path_to_ino: HashMap<String, u64>,
    next_ino: u64,
}

impl InodeTable {
    fn new(root_path: &str) -> Self {
        let mut t = Self {
            ino_to_path: HashMap::new(),
            path_to_ino: HashMap::new(),
            next_ino: 2,
        };
        t.ino_to_path.insert(1, root_path.to_string());
        t.path_to_ino.insert(root_path.to_string(), 1);
        t
    }

    fn get_or_create(&mut self, path: &str) -> u64 {
        if let Some(&ino) = self.path_to_ino.get(path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.ino_to_path.insert(ino, path.to_string());
        self.path_to_ino.insert(path.to_string(), ino);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<&str> {
        self.ino_to_path.get(&ino).map(|s| s.as_str())
    }

    fn rename(&mut self, old: &str, new: &str) {
        if let Some(ino) = self.path_to_ino.remove(old) {
            self.ino_to_path.insert(ino, new.to_string());
            self.path_to_ino.insert(new.to_string(), ino);
        }
    }

    fn remove(&mut self, path: &str) {
        if let Some(ino) = self.path_to_ino.remove(path) {
            self.ino_to_path.remove(&ino);
        }
    }
}

// ---------------------------------------------------------------------------
// RemoteFs
// ---------------------------------------------------------------------------

pub struct RemoteFs {
    backend: Arc<dyn RemoteBackend>,
    shared: Arc<Shared>,
    task_pool: Arc<ThreadPool>,
    /// Accessed only in `destroy` which still has `&mut self`.
    updater: Option<CacheUpdater>,
    inodes: Mutex<InodeTable>,
    /// Set to `true` by `destroy`; lets main detect an external unmount.
    unmounted: Arc<AtomicBool>,
    /// FUSE session notifier; populated by the caller after `spawn_mount2`.
    /// Used to push `inval_inode` messages to the kernel when a background
    /// directory fetch completes.
    notifier: Arc<Mutex<Option<Notifier>>>,
}

impl RemoteFs {
    pub fn new(
        remote_path: String,
        backend: Arc<dyn RemoteBackend>,
        shared: Arc<Shared>,
        task_pool: Arc<ThreadPool>,
    ) -> Self {
        let inodes = InodeTable::new(&remote_path);
        Self {
            backend,
            shared,
            task_pool,
            updater: None,
            inodes: Mutex::new(inodes),
            unmounted: Arc::new(AtomicBool::new(false)),
            notifier: Arc::new(Mutex::new(None)),
        }
    }

    pub fn unmounted_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.unmounted)
    }

    /// Returns a handle through which the FUSE session notifier can be
    /// installed after `spawn_mount2` returns the `BackgroundSession`.
    pub fn notifier_handle(&self) -> Arc<Mutex<Option<Notifier>>> {
        Arc::clone(&self.notifier)
    }

    pub fn start_cache_updater(&mut self, period_secs: u64) {
        let mut u = CacheUpdater::new(Arc::clone(&self.shared), period_secs);
        u.start();
        self.updater = Some(u);
    }

    // --- path helpers ---

    fn child_path(&self, parent: &str, name: &str) -> String {
        path_join(parent, name)
    }

    fn parent_path(path: &str) -> String {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string())
    }

    // --- stat helpers ---

    /// Returns `Some(Some(stat))` if cached, `Some(None)` if cached-absent,
    /// or `None` if not in cache.
    fn cached_stat(&self, path: &str) -> Option<Option<StatData>> {
        self.shared.attr_cache.get(path)
    }

    /// Fetch stat from the remote, update the attr cache, return the stat.
    fn remote_stat(&self, path: &str) -> Option<StatData> {
        match self.backend.stat(path) {
            Some(d) => {
                self.shared
                    .attr_cache
                    .put(path.to_string(), Some(d.clone()));
                Some(d)
            }
            None => {
                self.shared.attr_cache.put(path.to_string(), None);
                None
            }
        }
    }

    fn get_stat(&self, path: &str) -> Option<StatData> {
        match self.cached_stat(path) {
            Some(entry) => entry,
            None => self.remote_stat(path),
        }
    }

    /// Ensure the file at `path` is in the local disk cache.
    fn ensure_cached(&self, path: &str) {
        let cachefile = self.shared.disk_cache.cachefile(path);
        if cachefile.exists() {
            return;
        }
        if self
            .backend
            .download_to(path, &cachefile, self.shared.disk_cache.max_size)
        {
            self.shared.disk_cache.put(&cachefile);
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem trait
// ---------------------------------------------------------------------------

impl Filesystem for RemoteFs {
    fn destroy(&mut self) {
        // Stop the background refresh thread before saving so the snapshot
        // is not modified concurrently while we serialize.
        if let Some(mut u) = self.updater.take() {
            u.shutdown();
        }
        self.shared.save();
        self.unmounted.store(true, Ordering::Relaxed);
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };

        let path = self.child_path(&parent_path, name_str);
        match self.get_stat(&path) {
            Some(stat) => {
                let ino = self.inodes.lock().unwrap().get_or_create(&path);
                reply.entry(&TTL, &to_file_attr(ino, &stat), Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        match self.get_stat(&path) {
            Some(stat) => reply.attr(&TTL, &to_file_attr(ino.0, &stat)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        // Truncate: ensure the local cache file exists, then truncate it.
        if let Some(new_size) = size {
            let cachefile = self.shared.disk_cache.cachefile(&path);
            let _guard = self.shared.path_lock.lock(&path);
            self.ensure_cached(&path);
            if let Err(e) = std::fs::OpenOptions::new()
                .write(true)
                .open(&cachefile)
                .and_then(|f| f.set_len(new_size))
            {
                log::error!("truncate cache {}: {e}", cachefile.display());
            }
        }

        let time_secs = |t: &TimeOrNow| -> u64 {
            match t {
                TimeOrNow::SpecificTime(st) => {
                    st.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
                }
                TimeOrNow::Now => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            }
        };

        let change = AttrChange {
            size,
            uid,
            gid,
            perm: mode,
            atime: atime.as_ref().map(time_secs),
            mtime: mtime.as_ref().map(time_secs),
        };

        if let Err(e) = self.backend.setstat(&path, change) {
            log::error!("setstat {path}: {e}");
            reply.error(Errno::EIO);
            return;
        }

        self.shared.attr_cache.remove(&path);
        match self.get_stat(&path) {
            Some(stat) => reply.attr(&TTL, &to_file_attr(ino.0, &stat)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let cachefile = self.shared.disk_cache.cachefile(&path);
        self.shared.disk_cache.renew(&cachefile);

        // Fast path: cache file exists and we can lock it immediately.
        if let Some(_guard) = self.shared.path_lock.try_lock(&path) {
            if cachefile.exists() {
                match read_from_cache(&cachefile, offset, size as usize) {
                    Ok(data) => {
                        reply.data(&data);
                        return;
                    }
                    Err(e) => log::warn!("read cache {}: {e}", cachefile.display()),
                }
            }
        }

        // Kick off a background download if nothing else is already doing it.
        if !self.shared.path_lock.is_locked(&path) {
            let path_c = path.clone();
            let shared = Arc::clone(&self.shared);
            self.task_pool.spawn(move || {
                let _guard = shared.path_lock.lock(&path_c);
                let cachefile = shared.disk_cache.cachefile(&path_c);
                if !cachefile.exists() {
                    with_thread_backend(&shared.backend, |b| {
                        if b.download_to(&path_c, &cachefile, shared.disk_cache.max_size) {
                            shared.disk_cache.put(&cachefile);
                        }
                    });
                }
            });
        }

        // Fallback: read directly from the remote backend.
        match self.backend.read_at(&path, offset, size as usize) {
            Ok(data) => reply.data(&data),
            Err(e) => {
                log::error!("read_at {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let cachefile = self.shared.disk_cache.cachefile(&path);

        // Phase 1: write to local cache under lock.
        {
            let _guard = self.shared.path_lock.lock(&path);

            self.ensure_cached(&path);

            if let Err(e) = write_to_cache(&cachefile, offset, data) {
                log::error!("write cache {}: {e}", cachefile.display());
                reply.error(Errno::EIO);
                return;
            }

            // Update size in attr cache.
            if let Ok(meta) = cachefile.metadata() {
                if let Some(Some(mut stat)) = self.shared.attr_cache.get(&path) {
                    stat.size = meta.len();
                    self.shared.attr_cache.put(path.clone(), Some(stat));
                }
            }
        } // lock released

        // Phase 2: async write to remote (fire-and-forget).
        let data_owned = data.to_vec();
        let path_c = path.clone();
        let cachefile_c = cachefile.clone();
        let shared = Arc::clone(&self.shared);
        self.task_pool.spawn(move || {
            with_thread_backend(&shared.backend, |b| {
                if let Err(e) = b.async_write(&path_c, offset, &data_owned, &cachefile_c) {
                    log::error!("async write {path_c}: {e}");
                }
            });
        });

        self.shared.disk_cache.put(&cachefile);
        reply.written(data.len() as u32);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        let names = match self.shared.dir_cache.get(&path) {
            Some(cached) => cached,
            None => {
                // Cold cache: reply immediately with only "." and ".." to
                // release the kernel VFS lock, then fetch the real listing
                // in the background.
                let parent_path = Self::parent_path(&path);
                let parent_ino = self.inodes.lock().unwrap().get_or_create(&parent_path);
                for (i, (entry_ino, kind, name)) in [
                    (ino.0, FileType::Directory, "."),
                    (parent_ino, FileType::Directory, ".."),
                ]
                .iter()
                .enumerate()
                {
                    if (i as u64) >= offset
                        && reply.add(INodeNo(*entry_ino), (i + 1) as u64, *kind, *name)
                    {
                        break;
                    }
                }
                // ReplyDirectory carries no TTL; the kernel's cached listing
                // will be invalidated by the inval_inode notification below.
                // TTL_EMPTY (2 s) is the fallback window if notification fails —
                // the kernel entry TTL for this inode will expire and trigger a
                // fresh readdir.
                reply.ok();

                // Deduplicate: skip if a fetch for this path is already running.
                if !self
                    .shared
                    .fetching_dirs
                    .lock()
                    .unwrap()
                    .insert(path.clone())
                {
                    return;
                }

                let path_c = path.clone();
                let shared = Arc::clone(&self.shared);
                let notifier = Arc::clone(&self.notifier);
                let ino_val = ino.0;
                self.task_pool.spawn(move || {
                    with_thread_backend(&shared.backend, |b| {
                        match b.readdir(&path_c) {
                            Ok(entries) => {
                                let names: Vec<String> =
                                    entries.iter().map(|(n, _)| n.clone()).collect();
                                shared.dir_cache.put(path_c.clone(), names);
                                for (name, stat) in &entries {
                                    let child = path_join(&path_c, name);
                                    shared.attr_cache.put(child, Some(stat.clone()));
                                }
                                // Push kernel cache invalidation so the
                                // kernel re-issues readdir immediately.
                                if let Some(n) = notifier.lock().unwrap().as_ref() {
                                    if let Err(e) = n.inval_inode(INodeNo(ino_val), 0, 0) {
                                        log::warn!("inval_inode {path_c}: {e}");
                                    }
                                }
                            }
                            Err(e) => log::error!("background readdir {path_c}: {e}"),
                        }
                    });
                    shared.fetching_dirs.lock().unwrap().remove(&path_c);
                });
                return;
            }
        };

        // Hot path: dir cache is warm — build the full listing immediately.
        let parent_path = Self::parent_path(&path);
        let mut inodes = self.inodes.lock().unwrap();
        let parent_ino = inodes.get_or_create(&parent_path);
        let mut all: Vec<(u64, FileType, String)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (parent_ino, FileType::Directory, "..".into()),
        ];
        for name in &names {
            let child = path_join(&path, name);
            let child_ino = inodes.get_or_create(&child);
            let kind = self
                .shared
                .attr_cache
                .get(&child)
                .flatten()
                .map(|s| match s.perm & 0o170000 {
                    0o040000 => FileType::Directory,
                    0o120000 => FileType::Symlink,
                    _ => FileType::RegularFile,
                })
                .unwrap_or(FileType::RegularFile);
            all.push((child_ino, kind, name.clone()));
        }
        drop(inodes);

        for (i, (child_ino, kind, name)) in all.iter().enumerate() {
            if (i as u64) < offset {
                continue;
            }
            if reply.add(INodeNo(*child_ino), (i + 1) as u64, *kind, name.as_str()) {
                break;
            }
        }
        reply.ok();
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(ino.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        match self.backend.readlink(&path) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => {
                log::error!("readlink {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };

        let path = self.child_path(&parent_path, name_str);
        let cachefile = self.shared.disk_cache.cachefile(&path);

        let _guard = self.shared.path_lock.lock(&path);
        let _ = std::fs::File::create(&cachefile);

        match self.backend.create_file(&path, mode) {
            Ok(_) => {
                self.shared.attr_cache.remove(&path);
                self.shared.dir_cache.remove(&parent_path);
                let ino = self.inodes.lock().unwrap().get_or_create(&path);
                let now = SystemTime::now();
                let attr = FileAttr {
                    ino: INodeNo(ino),
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::RegularFile,
                    perm: (mode & 0o7777) as u16,
                    nlink: 1,
                    uid: req.uid(),
                    gid: req.gid(),
                    rdev: 0,
                    blksize: 512,
                    flags: 0,
                };
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(0),
                    FopenFlags::empty(),
                );
            }
            Err(e) => {
                log::error!("create {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::ENOENT);
            return;
        };

        let path = self.child_path(&parent_path, name_str);
        match self.backend.create_dir(&path, mode) {
            Ok(_) => {
                self.shared.attr_cache.remove(&path);
                self.shared.dir_cache.remove(&parent_path);
                let ino = self.inodes.lock().unwrap().get_or_create(&path);
                let now = SystemTime::now();
                let attr = FileAttr {
                    ino: INodeNo(ino),
                    size: 0,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::Directory,
                    perm: (mode & 0o7777) as u16,
                    nlink: 2,
                    uid: req.uid(),
                    gid: req.gid(),
                    rdev: 0,
                    blksize: 512,
                    flags: 0,
                };
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => {
                log::error!("mkdir {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = self.child_path(&parent_path, name_str);

        match self.backend.delete_file(&path) {
            Ok(_) => {
                let cachefile = self.shared.disk_cache.cachefile(&path);
                let _guard = self.shared.path_lock.lock(&path);
                self.shared.disk_cache.pop(&cachefile);
                self.shared.attr_cache.remove(&path);
                self.shared.dir_cache.remove(&parent_path);
                self.inodes.lock().unwrap().remove(&path);
                reply.ok();
            }
            Err(e) => {
                log::error!("unlink {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(name_str) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = self.child_path(&parent_path, name_str);

        match self.backend.delete_dir(&path) {
            Ok(_) => {
                self.shared.attr_cache.remove(&path);
                self.shared.dir_cache.remove(&parent_path);
                self.inodes.lock().unwrap().remove(&path);
                reply.ok();
            }
            Err(e) => {
                log::error!("rmdir {path}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (parent_path, newparent_path) = {
            let inodes = self.inodes.lock().unwrap();
            let pp = match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let np = match inodes.path_of(newparent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            (pp, np)
        };

        let (Some(name_str), Some(newname_str)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let old = self.child_path(&parent_path, name_str);
        let new = self.child_path(&newparent_path, newname_str);

        match self.backend.rename(&old, &new) {
            Ok(_) => {
                {
                    let _g = self.shared.path_lock.lock(&old);
                    self.shared
                        .disk_cache
                        .pop(&self.shared.disk_cache.cachefile(&old));
                    self.shared.attr_cache.remove(&old);
                    self.shared.dir_cache.remove(&parent_path);
                }
                {
                    let _g = self.shared.path_lock.lock(&new);
                    self.shared
                        .disk_cache
                        .pop(&self.shared.disk_cache.cachefile(&new));
                    self.shared.attr_cache.remove(&new);
                    self.shared.dir_cache.remove(&newparent_path);
                }
                self.inodes.lock().unwrap().rename(&old, &new);
                reply.ok();
            }
            Err(e) => {
                log::error!("rename {old} -> {new}: {e}");
                reply.error(Errno::EIO);
            }
        }
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let parent_path = {
            let inodes = self.inodes.lock().unwrap();
            match inodes.path_of(parent.0).map(str::to_string) {
                Some(p) => p,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };
        let Some(link_name_str) = link_name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let link_path = self.child_path(&parent_path, link_name_str);
        let target_str = target.to_string_lossy();

        match self.backend.symlink(target_str.as_ref(), &link_path) {
            Ok(_) => {
                self.shared.attr_cache.remove(&link_path);
                self.shared.dir_cache.remove(&parent_path);
                let ino = self.inodes.lock().unwrap().get_or_create(&link_path);
                let now = SystemTime::now();
                let attr = FileAttr {
                    ino: INodeNo(ino),
                    size: target_str.len() as u64,
                    blocks: 0,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    crtime: now,
                    kind: FileType::Symlink,
                    perm: 0o777,
                    nlink: 1,
                    uid: req.uid(),
                    gid: req.gid(),
                    rdev: 0,
                    blksize: 512,
                    flags: 0,
                };
                reply.entry(&TTL, &attr, Generation(0));
            }
            Err(e) => {
                log::error!("symlink {link_path}: {e}");
                reply.error(Errno::ENOTSUP);
            }
        }
    }

    // macOS and various tools send ioctl probes to discover filesystem capabilities.
    // We support none; ENOTTY ("inappropriate ioctl for device") is the correct response.
    fn ioctl(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: fuser::IoctlFlags,
        _cmd: u32,
        _in_data: &[u8],
        _out_size: u32,
        reply: ReplyIoctl,
    ) {
        reply.error(Errno::ENOTTY);
    }

    // Called when a file descriptor is closed on the client side (not fsync).
    // No action needed; acknowledge silently to suppress the warning.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // Drive and SFTP both have no directory-sync concept; acknowledge silently.
    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // Suppress "[Not Implemented]" warnings on macOS Finder access.
    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        // ENOATTR on macOS / ENODATA on Linux: "this attribute does not exist",
        // which is more correct than ENOTSUP ("xattrs unsupported entirely").
        #[cfg(target_os = "macos")]
        reply.error(Errno::from_i32(libc::ENOATTR));
        #[cfg(not(target_os = "macos"))]
        reply.error(Errno::from_i32(libc::ENODATA));
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        const BSIZE: u64 = 4096;
        let (blocks, bfree) = match self.backend.statfs() {
            Some((total, free)) => (total / BSIZE, free / BSIZE),
            None => (0, 0),
        };
        reply.statfs(blocks, bfree, bfree, 0, 0, BSIZE as u32, 255, BSIZE as u32);
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Build a child path from a parent directory path and an entry name.
/// Free function so it can be used inside closures that don't hold `&self`.
fn path_join(parent: &str, name: &str) -> String {
    if parent == "/" || parent.is_empty() {
        format!("/{name}")
    } else {
        format!("{}/{name}", parent.trim_end_matches('/'))
    }
}

// ---------------------------------------------------------------------------
// Thread-local worker backends for the task pool
// ---------------------------------------------------------------------------

thread_local! {
    static THREAD_BACKEND: RefCell<Option<Arc<dyn RemoteBackend>>> = const { RefCell::new(None) };
}

fn with_thread_backend<F: FnOnce(&dyn RemoteBackend)>(factory: &Arc<dyn RemoteBackend>, f: F) {
    THREAD_BACKEND.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if borrow.is_none() {
            match factory.new_worker() {
                Ok(w) => *borrow = Some(w),
                Err(e) => {
                    log::error!("worker connect: {e}");
                    return;
                }
            }
        }
        if let Some(ref b) = *borrow {
            f(b.as_ref());
        }
    });
}

// ---------------------------------------------------------------------------
// Cache file I/O helpers
// ---------------------------------------------------------------------------

fn read_from_cache(cachefile: &PathBuf, offset: u64, size: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(cachefile)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; size];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

fn write_to_cache(cachefile: &PathBuf, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(cachefile)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)
}
