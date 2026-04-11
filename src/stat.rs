//! Portable stat data and conversions between ssh2 and fuser types.

use fuser::{FileAttr, FileType, INodeNo};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Raw file attributes without an inode number; stored in the attr cache.
/// `None` in the cache means the path was looked up and found absent (ENOENT).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatData {
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    /// Full mode bits (type + permissions), e.g. 0o100644.
    pub perm: u32,
    pub atime: u64,
    pub mtime: u64,
}

pub fn from_ssh2_stat(s: &ssh2::FileStat) -> StatData {
    StatData {
        size: s.size.unwrap_or(0),
        uid: s.uid.unwrap_or(0),
        gid: s.gid.unwrap_or(0),
        perm: s.perm.unwrap_or(0o100644),
        atime: s.atime.unwrap_or(0),
        mtime: s.mtime.unwrap_or(0),
    }
}

pub fn to_file_attr(ino: u64, s: &StatData) -> FileAttr {
    let kind = match s.perm & 0o170000 {
        0o040000 => FileType::Directory,
        0o120000 => FileType::Symlink,
        0o010000 => FileType::NamedPipe,
        0o020000 => FileType::CharDevice,
        0o060000 => FileType::BlockDevice,
        0o140000 => FileType::Socket,
        _ => FileType::RegularFile,
    };

    let atime = unix_secs(s.atime);
    let mtime = unix_secs(s.mtime);

    FileAttr {
        ino: INodeNo(ino),
        size: s.size,
        blocks: s.size.div_ceil(512),
        atime,
        mtime,
        ctime: mtime,
        crtime: UNIX_EPOCH,
        kind,
        perm: (s.perm & 0o7777) as u16,
        nlink: if kind == FileType::Directory { 2 } else { 1 },
        uid: s.uid,
        gid: s.gid,
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn unix_secs(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}
