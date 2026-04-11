//! SSH/SFTP connection wrapper.
//!
//! `SftpConn` owns both a `Session` and the `Sftp` subsystem derived from it.
//! Field declaration order ensures `sftp` is dropped before `session`.

use anyhow::{Context, Result, anyhow};
use ssh2::{Session, Sftp};
use std::net::TcpStream;
use std::path::PathBuf;

/// Parameters needed to open an SSH connection.
#[derive(Clone, Debug)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key_filename: Option<PathBuf>,
    pub password: Option<String>,
}

pub struct SftpConn {
    // `sftp` must be declared before `session` so Rust drops it first,
    // closing the SFTP subsystem before closing the SSH session.
    sftp: Sftp,
    _session: Session,
}

// ssh2 marks both Sftp and Session as Send + Sync via unsafe impls.
unsafe impl Send for SftpConn {}
unsafe impl Sync for SftpConn {}

impl SftpConn {
    pub fn connect(cfg: &SshConfig) -> Result<Self> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let tcp =
            TcpStream::connect(&addr).with_context(|| format!("cannot TCP-connect to {addr}"))?;

        let mut session = Session::new().context("create SSH session")?;
        session.set_tcp_stream(tcp);
        session.handshake().context("SSH handshake")?;

        authenticate(&mut session, cfg)?;

        if !session.authenticated() {
            return Err(anyhow!(
                "SSH authentication failed for {}@{}",
                cfg.user,
                cfg.host
            ));
        }

        let sftp = session.sftp().context("open SFTP subsystem")?;
        Ok(Self {
            sftp,
            _session: session,
        })
    }

    pub fn sftp(&self) -> &Sftp {
        &self.sftp
    }

    /// Expand a remote path that starts with `~`.
    ///
    /// SFTP has no shell, so `~` is never expanded by the server.
    /// We resolve it by asking the server for the canonical path of `.`
    /// (the login home directory), then substitute.
    pub fn expand_remote_path(&self, path: &str) -> Result<String> {
        if !path.starts_with('~') {
            return Ok(path.to_string());
        }
        let home = self
            .sftp
            .realpath(std::path::Path::new("."))
            .context("SFTP realpath(.) to resolve remote home directory")?
            .to_string_lossy()
            .into_owned();

        let expanded = if path == "~" {
            home
        } else if let Some(rest) = path.strip_prefix("~/") {
            format!("{home}/{rest}")
        } else {
            // ~otheruser/… — leave to the server; unlikely over SFTP
            path.to_string()
        };
        Ok(expanded)
    }
}

fn authenticate(session: &mut Session, cfg: &SshConfig) -> Result<()> {
    // 1. Explicit key file
    if let Some(key) = &cfg.key_filename {
        session
            .userauth_pubkey_file(&cfg.user, None, key, cfg.password.as_deref())
            .with_context(|| format!("key auth with {}", key.display()))?;
        return Ok(());
    }

    // 2. SSH agent
    if session.userauth_agent(&cfg.user).is_ok() && session.authenticated() {
        return Ok(());
    }

    // 3. Password
    if let Some(pw) = &cfg.password {
        session
            .userauth_password(&cfg.user, pw)
            .context("password auth")?;
        return Ok(());
    }

    // 4. Default key files in ~/.ssh
    if let Ok(home) = std::env::var("HOME") {
        for name in &["id_ed25519", "id_ecdsa", "id_rsa"] {
            let key = PathBuf::from(&home).join(".ssh").join(name);
            if key.exists()
                && session
                    .userauth_pubkey_file(&cfg.user, None, &key, None)
                    .is_ok()
                && session.authenticated()
            {
                return Ok(());
            }
        }
    }

    Ok(())
}
