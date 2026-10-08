//! The in-app file chooser's only window onto the filesystem: list one
//! directory's entry names and metadata. It never reads file contents.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{EngineError, Result};

const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub symlink: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Shortcut {
    pub label: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub shortcuts: Vec<Shortcut>,
}

pub fn shortcuts(repo: Option<&Path>) -> Vec<Shortcut> {
    let mut list = Vec::new();
    let mut push = |label: &str, path: Option<PathBuf>| {
        if let Some(path) = path.filter(|p| p.is_dir()) {
            list.push(Shortcut { label: label.into(), path: path.display().to_string() });
        }
    };
    push("Home", dirs::home_dir());
    push("BMAC repository", repo.map(Path::to_path_buf));
    push("Desktop", dirs::desktop_dir());
    push("Documents", dirs::document_dir());
    push("Downloads", dirs::download_dir());
    push("Computer", Some(PathBuf::from("/")));
    list
}

pub fn list(path: Option<&str>, repo: Option<&Path>) -> Result<Listing> {
    let start = match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => PathBuf::from(p),
        None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
    };
    if !start.is_absolute() {
        return Err(EngineError::InvalidArgument("Enter an absolute path.".into()));
    }
    let dir = start.canonicalize().map_err(|e| EngineError::InvalidArgument(format!("{}: {e}", start.display())))?;
    let dir = if dir.is_dir() { dir } else { dir.parent().map(Path::to_path_buf).unwrap_or(dir) };
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(&dir).map_err(|e| EngineError::InvalidArgument(format!("{}: {e}", dir.display())))? {
        let Ok(item) = item else { continue };
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = item.file_name().to_string_lossy().to_string();
        let symlink = item.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        let meta = std::fs::metadata(item.path()).ok();
        let kind = match &meta {
            Some(m) if m.is_dir() => EntryKind::Directory,
            Some(m) if m.is_file() => EntryKind::File,
            _ => EntryKind::Other,
        };
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        entries.push(Entry {
            hidden: name.starts_with('.'),
            path: item.path().display().to_string(),
            size: meta.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
            name,
            kind,
            modified,
            symlink,
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Directory, a.name.to_lowercase()).cmp(&(b.kind != EntryKind::Directory, b.name.to_lowercase()))
    });
    Ok(Listing {
        parent: dir.parent().map(|p| p.display().to_string()),
        path: dir.display().to_string(),
        entries,
        truncated,
        shortcuts: shortcuts(repo),
    })
}

/// Write a run's log as plain text into the Downloads (or home) directory
/// under a generated name, never overwriting. Returns the new file's path.
pub fn export_log(name_hint: &str, lines: impl Iterator<Item = String>) -> Result<PathBuf> {
    use std::io::Write;
    let dir = dirs::download_dir()
        .filter(|d| d.is_dir())
        .or_else(dirs::home_dir)
        .ok_or_else(|| EngineError::Io("no Downloads or home directory".into()))?;
    let safe: String = name_hint.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    for n in 0..100 {
        let suffix = if n == 0 { String::new() } else { format!("-{n}") };
        let path = dir.join(format!("bmac-{safe}-{stamp}{suffix}.log"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let mut out = std::io::BufWriter::new(file);
                for line in lines {
                    writeln!(out, "{line}")?;
                }
                out.flush()?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(EngineError::Io("could not choose a log file name".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_directories_first_without_reading_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("a.iso"), b"1234").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"").unwrap();
        let listing = list(Some(dir.path().to_str().unwrap()), None).unwrap();
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["zdir", ".hidden", "a.iso"]);
        assert_eq!(listing.entries[2].size, Some(4));
        assert!(listing.entries[1].hidden);
        assert!(list(Some("relative"), None).is_err());
    }
}
