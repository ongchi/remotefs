//! SFTP implementation of [`super::RemoteBackend`].

mod conn;

pub use conn::{SftpConn, SshConfig};

use super::{AttrChange, RemoteBackend};
use crate::stat::{StatData, from_ssh2_stat};
use anyhow::{Context, Result};
use ssh2::OpenType;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct SftpBackend {
    conn: Mutex<SftpConn>,
    config: SshConfig,
}

impl SftpBackend {
    pub fn new(conn: SftpConn, config: SshConfig) -> Arc<Self> {
        Arc::new(Self {
            conn: Mutex::new(conn),
            config,
        })
    }
}

impl RemoteBackend for SftpBackend {
    fn stat(&self, path: &str) -> Option<StatData> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .lstat(Path::new(path))
            .ok()
            .map(|s| from_ssh2_stat(&s))
    }

    fn readdir(&self, path: &str) -> Result<Vec<(String, StatData)>> {
        let conn = self.conn.lock().unwrap();
        let entries = conn
            .sftp()
            .readdir(Path::new(path))
            .with_context(|| format!("readdir {path}"))?;
        Ok(entries
            .into_iter()
            .filter_map(|(p, s)| {
                p.file_name()
                    .map(|n| (n.to_string_lossy().into_owned(), from_ssh2_stat(&s)))
            })
            .filter(|(n, _)| !n.is_empty())
            .collect())
    }

    fn download_to(&self, path: &str, dest: &Path, max_bytes: u64) -> bool {
        let conn = self.conn.lock().unwrap();

        let size = match conn.sftp().lstat(Path::new(path)) {
            Ok(s) => s.size.unwrap_or(0),
            Err(e) => {
                log::error!("download_to lstat {path}: {e}");
                return false;
            }
        };
        if size > max_bytes {
            return false;
        }

        log::info!("download_to {path}");
        let tmp = PathBuf::from(format!("{}.tmp", dest.display()));

        let result: Result<()> = (|| {
            let mut remote =
                conn.sftp()
                    .open_mode(Path::new(path), ssh2::OpenFlags::READ, 0, OpenType::File)?;
            let mut local = std::fs::File::create(&tmp)?;
            std::io::copy(&mut remote, &mut local)?;
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
        let conn = self.conn.lock().unwrap();
        let mut f = conn
            .sftp()
            .open_mode(Path::new(path), ssh2::OpenFlags::READ, 0, OpenType::File)
            .with_context(|| format!("open {path}"))?;
        f.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek {path}"))?;
        let mut buf = vec![0u8; size];
        let n = f.read(&mut buf).with_context(|| format!("read {path}"))?;
        buf.truncate(n);
        Ok(buf)
    }

    fn async_write(&self, path: &str, offset: u64, data: &[u8], _local: &Path) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut f = conn
            .sftp()
            .open_mode(
                Path::new(path),
                ssh2::OpenFlags::WRITE | ssh2::OpenFlags::READ,
                0,
                OpenType::File,
            )
            .with_context(|| format!("open {path} for async write"))?;
        f.seek(SeekFrom::Start(offset))
            .with_context(|| format!("seek {path}"))?;
        f.write_all(data).with_context(|| format!("write {path}"))?;
        Ok(())
    }

    fn create_file(&self, path: &str, mode: u32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .open_mode(
                Path::new(path),
                ssh2::OpenFlags::WRITE | ssh2::OpenFlags::CREATE | ssh2::OpenFlags::TRUNCATE,
                mode as i32,
                OpenType::File,
            )
            .map(|_| ())
            .with_context(|| format!("create {path}"))
    }

    fn create_dir(&self, path: &str, mode: u32) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .mkdir(Path::new(path), mode as i32)
            .with_context(|| format!("mkdir {path}"))
    }

    fn delete_file(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .unlink(Path::new(path))
            .with_context(|| format!("unlink {path}"))
    }

    fn delete_dir(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .rmdir(Path::new(path))
            .with_context(|| format!("rmdir {path}"))
    }

    fn rename(&self, old: &str, new: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .rename(Path::new(old), Path::new(new), None)
            .with_context(|| format!("rename {old} -> {new}"))
    }

    fn setstat(&self, path: &str, change: AttrChange) -> Result<()> {
        let stat = ssh2::FileStat {
            size: change.size,
            uid: change.uid,
            gid: change.gid,
            perm: change.perm,
            atime: change.atime,
            mtime: change.mtime,
        };
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .setstat(Path::new(path), stat)
            .with_context(|| format!("setstat {path}"))
    }

    fn symlink(&self, target: &str, linkname: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .symlink(Path::new(linkname), Path::new(target))
            .with_context(|| format!("symlink {linkname} -> {target}"))
    }

    fn readlink(&self, path: &str) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        conn.sftp()
            .readlink(Path::new(path))
            .map(|p| p.to_string_lossy().into_owned())
            .with_context(|| format!("readlink {path}"))
    }

    fn new_worker(&self) -> Result<Arc<dyn RemoteBackend>> {
        let conn = SftpConn::connect(&self.config).context("open worker SFTP connection")?;
        Ok(SftpBackend::new(conn, self.config.clone()))
    }
}
