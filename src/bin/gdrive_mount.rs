//! Mount Google Drive as a local FUSE filesystem.
//!
//! ## Quick start
//!
//! 1. Create a Google Cloud project, enable the Drive API, and download an
//!    OAuth2 "Desktop app" credentials file from the Cloud Console.
//!    See: <https://console.cloud.google.com/apis/credentials>
//!
//! 2. On first run, a browser window opens asking you to authorize access.
//!    The token is stored at `~/.config/remotefs/gdrive_token_<email>.json`
//!    so subsequent runs are silent.
//!
//! ## Usage
//!
//!     gdrive_mount [OPTIONS] ACCOUNT[:DRIVE_PATH] MOUNTPOINT
//!
//! ## Examples
//!
//!     # Mount all of My Drive
//!     gdrive_mount alice@gmail.com /mnt/gdrive
//!
//!     # Mount only the "Projects" folder
//!     gdrive_mount alice@gmail.com:/Projects /mnt/projects
//!
//!     # Two accounts simultaneously
//!     gdrive_mount alice@gmail.com /mnt/personal
//!     gdrive_mount work@company.com /mnt/work

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use remotefs::backend::gdrive::DriveBackend;
use remotefs::mount::{absolute_path, daemonize_if, mount_and_run};
use remotefs::RemoteFs;
use remotefs::shared::Shared;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "gdrive_mount",
    about = "Mount Google Drive as a local FUSE filesystem",
    override_usage = "gdrive_mount [OPTIONS] ACCOUNT[:DRIVE_PATH] MOUNTPOINT"
)]
struct Args {
    /// Google account and optional Drive path: ACCOUNT[:DRIVE_PATH]
    ///
    /// ACCOUNT is the Gmail / Google Workspace address used to authorize.
    /// DRIVE_PATH selects a sub-folder as the mount root (default: /).
    ///
    /// Examples: alice@gmail.com   alice@gmail.com:/Projects
    #[arg(value_name = "ACCOUNT[:DRIVE_PATH]")]
    target: String,

    /// Local directory to mount on
    #[arg(value_name = "MOUNTPOINT")]
    mount_point: PathBuf,

    /// Path to a credentials.json file from the Google Cloud Console.
    ///
    /// Defaults to ~/.config/remotefs/gdrive_credentials.json if not given.
    #[arg(long = "credentials", short = 'k', value_name = "FILE")]
    credentials: Option<PathBuf>,

    /// Local directory for cached files
    #[arg(short = 'c', long = "cache-path", value_name = "PATH")]
    cache_path: Option<PathBuf>,

    /// Maximum disk cache size in MiB
    #[arg(
        short = 's',
        long = "cache-size",
        default_value_t = 1024,
        value_name = "MiB"
    )]
    cache_size_mb: u64,

    /// Seconds between background cache refreshes
    #[arg(
        short = 't',
        long = "cache-timeout",
        default_value_t = 30,
        value_name = "SECS"
    )]
    cache_timeout: u64,

    /// Worker thread count (default: 4 × CPU cores)
    #[arg(short = 'j', long = "parallel", value_name = "N")]
    parallel: Option<usize>,

    /// Mount a Shared Drive (Google Workspace) instead of My Drive.
    ///
    /// Pass the alphanumeric Shared Drive ID from the Drive URL.
    /// Example: gdrive_mount alice@corp.com /mnt/shared --shared-drive 0ABCdef123
    #[arg(long = "shared-drive", value_name = "DRIVE_ID")]
    shared_drive: Option<String>,

    /// Enable background cache refresh thread
    #[arg(long)]
    auto_cache: bool,

    /// Stay in the foreground (do not daemonize)
    #[arg(short = 'f', long)]
    foreground: bool,

    /// Verbose (info-level) logging
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let filter = if args.verbose { "info" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter)).init();

    let (email, drive_path) = parse_target(&args.target)?;

    let creds_path = args.credentials.unwrap_or_else(default_credentials_path);
    if !creds_path.exists() {
        anyhow::bail!(
            "credentials file not found: {}\n\
             \n\
             Create one at https://console.cloud.google.com/apis/credentials\n\
             (choose \"Desktop app\", download JSON, save it to that path)",
            creds_path.display()
        );
    }

    let mount_point = absolute_path(&args.mount_point);
    let cache_path = args
        .cache_path
        .map(|p| absolute_path(&p))
        .unwrap_or_else(|| default_cache_path(&email));

    let created_mount_point = !mount_point.exists();
    if created_mount_point {
        std::fs::create_dir_all(&mount_point)
            .with_context(|| format!("create mount point {}", mount_point.display()))?;
    }

    eprintln!("Connecting to Google Drive as {email}…");
    let backend = DriveBackend::connect(&creds_path, &email, args.shared_drive.as_deref())?;

    let shared = Arc::new(Shared::new(
        Arc::clone(&backend) as _,
        cache_path,
        args.cache_size_mb,
    )?);

    let parallel = args.parallel.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(4)
            * 4
    });

    // Fork before creating thread pools so the fork happens while
    // the process is single-threaded.
    let ready = daemonize_if(!args.foreground)?;

    let task_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .thread_name(|i| format!("gdrive-pool-{i}"))
            .build()?,
    );

    let mut fs = RemoteFs::new(drive_path, backend as _, Arc::clone(&shared), task_pool);

    if args.auto_cache {
        fs.start_cache_updater(args.cache_timeout);
    }

    mount_and_run(fs, &mount_point, "gdrive", created_mount_point, ready)
}

// ---------------------------------------------------------------------------
// Target parsing
// ---------------------------------------------------------------------------

/// Parse `ACCOUNT[:DRIVE_PATH]` into `(email, drive_path)`.
///
/// ```text
/// alice@gmail.com            →  ("alice@gmail.com", "/")
/// alice@gmail.com:           →  ("alice@gmail.com", "/")
/// alice@gmail.com:/Projects  →  ("alice@gmail.com", "/Projects")
/// ```
fn parse_target(target: &str) -> Result<(String, String)> {
    let (account, path) = match target.split_once(':') {
        Some((a, p)) => (a, if p.is_empty() { "/" } else { p }),
        None => (target, "/"),
    };

    if !account.contains('@') {
        return Err(anyhow!(
            "target must be a Google account address (e.g. alice@gmail.com), got {:?}",
            account
        ));
    }

    Ok((account.to_string(), normalise_drive_path(path)))
}

// ---------------------------------------------------------------------------
// Path / default helpers
// ---------------------------------------------------------------------------

/// Ensure the drive path starts with `/` and has no trailing slash.
fn normalise_drive_path(path: &str) -> String {
    let p = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if p.len() > 1 {
        p.trim_end_matches('/').to_string()
    } else {
        p
    }
}

fn default_credentials_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".config/remotefs/gdrive_credentials.json")
}

/// Cache is scoped per account so two mounted accounts don't share cached files.
fn default_cache_path(email: &str) -> PathBuf {
    let safe = email.replace(['@', '.'], "_");
    let base = {
        #[cfg(target_os = "macos")]
        {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
                .join("Library/Caches")
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::env::var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| PathBuf::from(h).join(".cache"))
                        .unwrap_or_else(|_| PathBuf::from("/tmp"))
                })
        }
    };
    base.join("remotefs").join(format!("gdrive_{safe}"))
}
