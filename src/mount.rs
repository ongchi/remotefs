//! Shared FUSE mount loop used by both `ssh_mount` and `gdrive_mount`.

use crate::RemoteFs;
use anyhow::Result;
use fuser::{Config, MountOption, SessionACL};
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
pub struct ReadyNotifier {
    fd: Option<RawFd>,
}

impl ReadyNotifier {
    fn noop() -> Self {
        Self { fd: None }
    }

    pub fn is_daemon(&self) -> bool {
        self.fd.is_some()
    }

    /// Write the "ready" byte to the parent and redirect stdio to `/dev/null`.
    pub fn notify(self) {
        let Some(fd) = self.fd else { return };
        unsafe {
            let ok: u8 = 0;
            libc::write(fd, &ok as *const u8 as *const libc::c_void, 1);
            libc::close(fd);
            // Redirect stdio so the daemon doesn't produce unexpected output.
            let devnull =
                libc::open(c"/dev/null".as_ptr() as *const libc::c_char, libc::O_RDWR);
            if devnull >= 0 {
                libc::dup2(devnull, 0);
                libc::dup2(devnull, 1);
                libc::dup2(devnull, 2);
                libc::close(devnull);
            }
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
            Ok(ReadyNotifier { fd: Some(write_fd) })
        }
        _ => {
            // Parent: wait for child to signal success, then exit.
            unsafe { libc::close(write_fd) };
            let mut byte: u8 = 1;
            let n = unsafe {
                libc::read(read_fd, &mut byte as *mut u8 as *mut libc::c_void, 1)
            };
            unsafe { libc::close(read_fd) };
            if n == 1 && byte == 0 {
                std::process::exit(0);
            } else {
                eprintln!("Daemon failed to mount.");
                std::process::exit(1);
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
    ready: ReadyNotifier,
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
    let _session = fuser::spawn_mount2(fs, mount_point, &config)?;
    // Install the notifier so background readdir fetches can push
    // inval_inode messages to the kernel.
    *notifier_handle.lock().unwrap() = Some(_session.notifier());

    let is_daemon = ready.is_daemon();
    // Signal the parent that the mount is live (no-op when not daemonizing).
    // Also redirects stdio to /dev/null in daemon mode.
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
