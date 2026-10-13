// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A minimal ustar writer for the first-boot payload and the discovery
//! overlay: files, directories, and symlinks with short names, owned by
//! root, which GNU tar and busybox tar extract.

const BLOCK: usize = 512;

#[derive(Debug, Default)]
pub struct TarWriter {
    out: Vec<u8>,
    mtime: u64,
}

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(text.as_bytes());
    field[width] = 0;
}

impl TarWriter {
    pub fn new(mtime_s: u64) -> TarWriter {
        TarWriter {
            out: Vec::new(),
            mtime: mtime_s,
        }
    }

    /// Adds a regular file. Names are relative, at most 100 bytes, without
    /// `..`.
    pub fn file(&mut self, name: &str, mode: u32, data: &[u8]) {
        self.entry(name, mode, b'0', "", data);
    }

    /// Adds a directory; `name` has no trailing slash.
    pub fn dir(&mut self, name: &str, mode: u32) {
        self.entry(&format!("{name}/"), mode, b'5', "", b"");
    }

    pub fn symlink(&mut self, name: &str, target: &str) {
        assert!(target.len() <= 100, "long link target {target:?}");
        self.entry(name, 0o777, b'2', target, b"");
    }

    fn entry(&mut self, name: &str, mode: u32, kind: u8, link: &str, data: &[u8]) {
        let bare = name.strip_suffix('/').unwrap_or(name);
        assert!(
            !bare.is_empty()
                && name.len() <= 100
                && !bare.starts_with('/')
                && !bare.split('/').any(|part| part == ".." || part.is_empty()),
            "bad payload name {name:?}"
        );
        let mut header = [0u8; BLOCK];
        header[..name.len()].copy_from_slice(name.as_bytes());
        octal(&mut header[100..108], u64::from(mode));
        octal(&mut header[108..116], 0);
        octal(&mut header[116..124], 0);
        octal(&mut header[124..136], data.len() as u64);
        octal(&mut header[136..148], self.mtime);
        header[148..156].fill(b' ');
        header[156] = kind;
        header[157..157 + link.len()].copy_from_slice(link.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[265..269].copy_from_slice(b"root");
        header[297..301].copy_from_slice(b"root");
        let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
        let text = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(text.as_bytes());
        self.out.extend_from_slice(&header);
        self.out.extend_from_slice(data);
        let pad = (BLOCK - data.len() % BLOCK) % BLOCK;
        self.out.extend(std::iter::repeat_n(0, pad));
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.out.extend(std::iter::repeat_n(0, 2 * BLOCK));
        self.out
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn gnu_tar_reads_it_back() {
        let mut tar = TarWriter::new(1_760_000_000);
        tar.file("a.txt", 0o600, b"hello\n");
        tar.file("dir/b.bin", 0o644, &[0u8; 1000]);
        tar.file("dir/empty", 0o600, b"");
        tar.dir("etc/runlevels", 0o755);
        tar.symlink("etc/runlevels/local", "/etc/init.d/local");
        let bytes = tar.finish();
        assert_eq!(bytes.len() % BLOCK, 0);
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("p.tar");
        std::fs::write(&archive, &bytes).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let status = Command::new("tar")
            .arg("-x")
            .arg("--no-same-owner")
            .arg("-C")
            .arg(&out)
            .arg("-f")
            .arg(&archive)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"hello\n");
        assert_eq!(std::fs::read(out.join("dir/b.bin")).unwrap().len(), 1000);
        assert!(std::fs::read(out.join("dir/empty")).unwrap().is_empty());
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(out.join("a.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::read_link(out.join("etc/runlevels/local")).unwrap(),
            std::path::Path::new("/etc/init.d/local")
        );
    }

    #[test]
    #[should_panic(expected = "bad payload name")]
    fn names_cannot_escape() {
        TarWriter::new(0).file("../etc/passwd", 0o600, b"");
    }
}
