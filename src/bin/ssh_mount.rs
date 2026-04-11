//! Mount a remote directory over SFTP as a local FUSE filesystem.
//!
//! ## Quick start
//!
//! 1. Ensure your SSH key or password is set up for the target host.
//!    Host aliases, `HostName`, `Port`, `User`, and `IdentityFile` are
//!    resolved automatically from `~/.ssh/config`.
//!
//! ## Usage
//!
//!     ssh_mount [OPTIONS] [USER@]HOST[:REMOTE_PATH] MOUNTPOINT
//!
//! ## Examples
//!
//!     # Mount the root of a host defined in ~/.ssh/config
//!     ssh_mount myserver /mnt/remote
//!
//!     # Mount a specific path with an explicit key
//!     ssh_mount root@192.168.1.1:/data /mnt/data -i ~/.ssh/id_ed25519
//!
//!     # Expand the remote home directory with ~
//!     ssh_mount gce:~/projects /mnt/gce

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use remotefs::RemoteFs;
use remotefs::mount::{absolute_path, mount_and_run};
use remotefs::backend::sftp::{SftpBackend, SftpConn, SshConfig};
use remotefs::shared::Shared;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "ssh_mount",
    about = "Mount a remote directory over SFTP as a local FUSE filesystem",
    override_usage = "ssh_mount [OPTIONS] [USER@]HOST[:REMOTE_PATH] MOUNTPOINT"
)]
struct Args {
    /// Remote target: [user@]host[:remote_path]
    ///
    /// HOST may be a bare hostname, IP address, or an alias from ~/.ssh/config.
    /// REMOTE_PATH defaults to / if omitted. A leading ~ is expanded to the
    /// remote home directory via SFTP realpath.
    ///
    /// Examples: myserver   root@192.168.1.1:/data   gce:~/projects
    #[arg(value_name = "[USER@]HOST[:PATH]")]
    target: String,

    /// Local mount point
    #[arg(value_name = "MOUNTPOINT")]
    mount_point: PathBuf,

    /// SSH port (default: from ~/.ssh/config, then 22)
    #[arg(short = 'p', long = "port", value_name = "PORT")]
    ssh_port: Option<u16>,

    /// SSH identity file (private key)
    #[arg(short = 'i', long = "identity", value_name = "FILE")]
    ssh_key: Option<PathBuf>,

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

    /// Enable background cache refresh thread
    #[arg(long)]
    auto_cache: bool,

    /// Verbose (info-level) logging
    #[arg(short = 'v', long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let filter = if args.verbose { "info" } else { "warn" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter)).init();

    let (cli_user, alias, remote_path) = parse_target(&args.target)?;
    let resolved = SshHostParams::resolve(&alias);

    let host = resolved.hostname.unwrap_or_else(|| alias.clone());
    let port = args.ssh_port.or(resolved.port).unwrap_or(22);
    let user = cli_user
        .or(resolved.user)
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_else(|| "root".to_string());
    let key_filename = args.ssh_key.or(resolved.identity_file);

    let ssh_config = SshConfig {
        host,
        port,
        user: user.clone(),
        key_filename,
        password: None,
    };

    let cache_path = args
        .cache_path
        .map(|p| absolute_path(&p))
        .unwrap_or_else(|| default_cache_path(&user, &ssh_config.host));
    let mount_point = absolute_path(&args.mount_point);

    let created_mount_point = !mount_point.exists();
    if created_mount_point {
        std::fs::create_dir_all(&mount_point)
            .with_context(|| format!("create mount point {}", mount_point.display()))?;
    }

    let conn = SftpConn::connect(&ssh_config)?;
    let remote_path = conn.expand_remote_path(&remote_path)?;

    let backend = SftpBackend::new(conn, ssh_config);
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

    let task_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel)
            .thread_name(|i| format!("sftp-pool-{i}"))
            .build()?,
    );

    let mut fs = RemoteFs::new(remote_path, backend as _, Arc::clone(&shared), task_pool);

    if args.auto_cache {
        fs.start_cache_updater(args.cache_timeout);
    }

    mount_and_run(fs, &mount_point, "sftp", created_mount_point)
}

// ---------------------------------------------------------------------------
// Target parsing
// ---------------------------------------------------------------------------

/// Parse `[user@]host[:remote_path]` into `(Option<user>, host_alias, remote_path)`.
fn parse_target(target: &str) -> Result<(Option<String>, String, String)> {
    let (userhost, remote_path) = match target.split_once(':') {
        Some((uh, path)) => (uh, if path.is_empty() { "/" } else { path }.to_string()),
        None => (target, "/".to_string()),
    };

    let (user, alias) = match userhost.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, userhost.to_string()),
    };

    if alias.is_empty() {
        return Err(anyhow!("host must not be empty (got {:?})", target));
    }
    Ok((user, alias, remote_path))
}

// ---------------------------------------------------------------------------
// SSH config resolution
// ---------------------------------------------------------------------------

struct SshHostParams {
    hostname: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    identity_file: Option<PathBuf>,
}

impl SshHostParams {
    fn resolve(alias: &str) -> Self {
        let mut params = Self {
            hostname: None,
            port: None,
            user: None,
            identity_file: None,
        };

        let output = match std::process::Command::new("ssh")
            .args(["-G", alias])
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            _ => return params,
        };

        for line in String::from_utf8_lossy(&output).lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "hostname" => params.hostname = Some(value.to_string()),
                "port" => params.port = value.parse().ok(),
                "user" => params.user = Some(value.to_string()),
                "identityfile" => {
                    if params.identity_file.is_none() {
                        let p = expand_tilde(value);
                        if p.exists() {
                            params.identity_file = Some(p);
                        }
                    }
                }
                _ => {}
            }
        }

        params
    }
}

// ---------------------------------------------------------------------------
// Path utilities
// ---------------------------------------------------------------------------

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn default_cache_path(user: &str, host: &str) -> PathBuf {
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
    base.join("remotefs").join(format!("{user}@{host}"))
}
