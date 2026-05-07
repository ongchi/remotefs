//! Shared FUSE mount loop used by both `ssh_mount` and `gdrive_mount`.

use crate::RemoteFs;
use anyhow::Result;
use fuser::{Config, MountOption, SessionACL};
use std::cell::Cell;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Daemonize support
// ---------------------------------------------------------------------------

/// Passed to `mount_and_run`. The child calls `notify()` once the FUSE session
/// is live; the waiting parent then exits, returning the shell prompt.
///
/// Uses `Cell` for interior mutability so `notify`/`fail` can be called via
/// `&self` references (e.g. inside `map_err` closures).
pub struct ReadyNotifier {
    fd: Cell<Option<RawFd>>,
}

impl ReadyNotifier {
    fn noop() -> Self {
        Self {
            fd: Cell::new(None),
        }
    }

    pub fn is_daemon(&self) -> bool {
        self.fd.get().is_some()
    }

    /// Signal success to the parent and redirect stdio to `/dev/null`.
    pub fn notify(&self) {
        let Some(fd) = self.fd.take() else { return };
        unsafe {
            // Byte 0 = success.
            let ok: u8 = 0;
            libc::write(fd, &ok as *const u8 as *const libc::c_void, 1);
            libc::close(fd);
            let devnull = libc::open(c"/dev/null".as_ptr() as *const libc::c_char, libc::O_RDWR);
            if devnull >= 0 {
                libc::dup2(devnull, 0);
                libc::dup2(devnull, 1);
                libc::dup2(devnull, 2);
                libc::close(devnull);
            }
        }
    }

    /// Signal failure to the parent with a human-readable message.
    pub fn fail(&self, msg: &str) {
        let Some(fd) = self.fd.take() else { return };
        // Write header + message as a single syscall so the parent always reads
        // the complete payload in one read() call (POSIX atomic for < PIPE_BUF).
        let mut buf = Vec::with_capacity(1 + msg.len());
        buf.push(1u8); // Byte 1 = error
        buf.extend_from_slice(msg.as_bytes());
        unsafe {
            libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
            libc::close(fd);
        }
    }
}

impl Drop for ReadyNotifier {
    fn drop(&mut self) {
        // fd still open means neither notify() nor fail() was called.
        // Close it so the parent's read() returns EOF instead of hanging.
        if let Some(fd) = self.fd.take() {
            unsafe { libc::close(fd) };
        }
    }
}

/// Fork if `enabled`. Returns a `ReadyNotifier` in the child only; the parent
/// blocks until the child calls `notify()`, then exits.
///
/// Call this **before** creating any thread pools so the fork happens while
/// the process is still single-threaded.
pub fn daemonize_if(enabled: bool) -> Result<ReadyNotifier> {
    if !enabled {
        return Ok(ReadyNotifier::noop());
    }

    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        anyhow::bail!("pipe: {}", std::io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);

    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            anyhow::bail!("fork: {}", std::io::Error::last_os_error());
        }
        0 => {
            // Child: close read end, become session leader, continue.
            unsafe { libc::close(read_fd) };
            unsafe { libc::setsid() };
            Ok(ReadyNotifier {
                fd: Cell::new(Some(write_fd)),
            })
        }
        _ => {
            // Parent: wait for child to signal success, then exit.
            unsafe { libc::close(write_fd) };
            let mut buf = [0u8; 1025];
            let n =
                unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            unsafe { libc::close(read_fd) };
            match (n, buf.first()) {
                (1.., Some(&0)) => std::process::exit(0),
                (1.., Some(&1)) => {
                    let msg = if n > 1 {
                        std::str::from_utf8(&buf[1..n as usize]).unwrap_or("(non-UTF-8)")
                    } else {
                        "(no details)"
                    };
                    eprintln!("Daemon failed to mount: {msg}");
                    eprintln!("Tip: run with -f/--foreground to see the full error output.");
                    std::process::exit(1);
                }
                _ => {
                    eprintln!(
                        "Daemon failed to mount (child exited without signalling).\n\
                         Tip: run with -f/--foreground to see full output.\n\
                         If the mount point is busy, run: umount <mountpoint>"
                    );
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mount loop
// ---------------------------------------------------------------------------

pub fn mount_and_run(
    fs: RemoteFs,
    mount_point: &std::path::Path,
    fs_name: &str,
    created_mount_point: bool,
    ready: &ReadyNotifier,
) -> Result<()> {
    let vol_name = mount_point
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| fs_name.to_string());

    let mut mount_options = vec![
        MountOption::FSName(fs_name.to_string()),
        MountOption::AutoUnmount,
        MountOption::DefaultPermissions,
    ];

    #[cfg(target_os = "macos")]
    {
        // local   — treat as a local disk; suppresses "network volume" dialogs and
        //           enables proper Open/Save panel behaviour in Finder.
        // noappledouble / noapplexattr — prevent ._-file and xattr noise on the remote.
        mount_options.push(MountOption::CUSTOM("local".to_string()));
        mount_options.push(MountOption::CUSTOM("noappledouble".to_string()));
        mount_options.push(MountOption::CUSTOM("noapplexattr".to_string()));
        mount_options.push(MountOption::CUSTOM(format!("volname={vol_name}")));
    }

    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
    }

    let mut config = Config::default();
    config.mount_options = mount_options;
    config.acl = SessionACL::All;

    let unmounted = fs.unmounted_flag();
    let notifier_handle = fs.notifier_handle();
    let _session = match fuser::spawn_mount2(fs, mount_point, &config) {
        Ok(s) => s,
        Err(e) => {
            let hint = if e.raw_os_error() == Some(libc::EBUSY) {
                format!("{e} — try: umount {}", mount_point.display())
            } else {
                e.to_string()
            };
            ready.fail(&hint);
            return Err(e.into());
        }
    };
    *notifier_handle.lock().unwrap() = Some(_session.notifier());

    let is_daemon = ready.is_daemon();
    ready.notify();

    if !is_daemon {
        eprintln!("Mounted on {}", mount_point.display());
    }

    while !SHUTDOWN.load(Ordering::Relaxed) && !unmounted.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    if unmounted.load(Ordering::Relaxed) {
        eprintln!("Unmounted externally, exiting.");
        std::mem::forget(_session);
    } else {
        eprintln!("Signal received, unmounting {}…", mount_point.display());
        drop(_session); // unmount before touching the mount-point directory
    }

    if created_mount_point {
        if let Err(e) = std::fs::remove_dir(mount_point) {
            eprintln!(
                "Warning: could not remove mount point {}: {e}",
                mount_point.display()
            );
        }
    }

    Ok(())
}

pub fn absolute_path(p: &PathBuf) -> PathBuf {
    if p.is_absolute() {
        p.clone()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(p)
    }
}
