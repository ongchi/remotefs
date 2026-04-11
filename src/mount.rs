//! Shared FUSE mount loop used by both `ssh_mount` and `gdrive_mount`.

use crate::RemoteFs;
use anyhow::Result;
use fuser::{Config, MountOption, SessionACL};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

pub fn mount_and_run(
    fs: RemoteFs,
    mount_point: &std::path::Path,
    fs_name: &str,
    created_mount_point: bool,
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
    eprintln!("Mounted on {}", mount_point.display());

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
