//! Background thread that periodically refreshes cached attrs and directory
//! listings from the remote server.

use crate::backend::RemoteBackend;
use crate::shared::Shared;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub struct CacheUpdater {
    shared: Arc<Shared>,
    period: Duration,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CacheUpdater {
    pub fn new(shared: Arc<Shared>, period_secs: u64) -> Self {
        Self {
            shared,
            period: Duration::from_secs(period_secs),
            running: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }

    pub fn start(&mut self) {
        let shared = Arc::clone(&self.shared);
        let period = self.period;
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        let handle = std::thread::Builder::new()
            .name("cache-updater".into())
            .spawn(move || {
                let worker = match shared.backend.new_worker() {
                    Ok(w) => w,
                    Err(e) => {
                        log::error!("cache-updater: connect failed: {e}");
                        return;
                    }
                };
                while running.load(Ordering::Relaxed) {
                    std::thread::sleep(period);
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }
                    renew_attrs(worker.as_ref(), &shared);
                    renew_dirs(worker.as_ref(), &shared);
                }
            })
            .expect("spawn cache-updater");

        self.thread = Some(handle);
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn renew_attrs(backend: &dyn RemoteBackend, shared: &Shared) {
    for (path, cached) in shared.attr_cache.snapshot() {
        let Some(_guard) = shared.path_lock.try_lock(&path) else {
            continue;
        };

        let remote = backend.stat(&path);

        match (&cached, &remote) {
            // File gone remotely: evict from disk cache and mark absent.
            (_, None) => {
                shared.disk_cache.pop(&shared.disk_cache.cachefile(&path));
                shared.attr_cache.put(path.clone(), None);
            }
            // Size changed: evict disk cache so the next read re-downloads.
            (Some(old), Some(new)) if old.size != new.size => {
                shared.disk_cache.pop(&shared.disk_cache.cachefile(&path));
                shared.attr_cache.put(path.clone(), Some(new.clone()));
            }
            // Otherwise just refresh attrs.
            (_, Some(new)) => {
                shared.attr_cache.put(path.clone(), Some(new.clone()));
            }
        }
    }
}

fn renew_dirs(backend: &dyn RemoteBackend, shared: &Shared) {
    for (path, cached) in shared.dir_cache.snapshot() {
        match backend.readdir(&path) {
            Ok(entries) => {
                let mut remote: Vec<String> = entries.into_iter().map(|(name, _)| name).collect();

                let mut local = cached.clone();
                local.sort_unstable();
                remote.sort_unstable();

                if local != remote {
                    shared.dir_cache.put(path, remote);
                }
            }
            Err(e) => log::debug!("cache-updater readdir {path}: {e}"),
        }
    }
}
