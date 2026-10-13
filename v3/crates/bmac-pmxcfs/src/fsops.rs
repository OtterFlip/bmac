// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The mutating filesystem calls, and reads, behind a trait so tests can
//! stand in for pmxcfs: refuse what it doesn't support, and miss a file it
//! is replacing.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::fs::{AtFlags, CWD, Timespec, Timestamps, utimensat};

pub trait FsOps: Send + Sync {
    /// Creates one directory; fails if it exists.
    fn mkdir(&self, path: &Path, mode: u32) -> io::Result<()> {
        std::fs::DirBuilder::new().mode(mode).create(path)
    }

    fn rmdir(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    fn unlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    /// `O_WRONLY | O_CREAT | O_TRUNC | O_NOFOLLOW`, never `O_EXCL`.
    fn create(&self, path: &Path, mode: u32) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
    }

    fn fsync(&self, file: &File, _path: &Path) -> io::Result<()> {
        file.sync_all()
    }

    fn fsync_dir(&self, path: &Path) -> io::Result<()> {
        File::open(path)?.sync_all()
    }

    fn chmod(&self, path: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// Sets a path's access and modification times to the epoch, which
    /// pmxcfs treats as a request to remove a stale lock directory.
    fn utime_zero(&self, path: &Path) -> io::Result<()> {
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        utimensat(
            CWD,
            path,
            &Timestamps {
                last_access: zero,
                last_modification: zero,
            },
            AtFlags::empty(),
        )
        .map_err(io::Error::from)
    }
}

/// The real filesystem.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl FsOps for RealFs {}
