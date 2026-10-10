//! The BMAC config directory: the operator's cluster.conf, moxN.conf, and
//! secrets.env beside the `*_dot_*` examples they start from, plus the
//! scripts' artifacts/. A checkout always uses its own config/. An installed
//! dashboard keeps it in its settings directory, or wherever the one-line
//! `config-location` file there points; scripts/lib/config.sh resolves it the
//! same way, so both must change together.
//!
//! The frontend reaches files here only by bare name. Only `*.conf`,
//! `*_dot_*`, and `*.env` names are visible, only `*.conf` files can be
//! written, and `*.env` contents are never returned: secrets.env is compared
//! with its example in-process and nothing else.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{EngineError, Result};

/// The file in the settings directory that relocates an installed config directory.
pub const POINTER_FILE: &str = "config-location";
/// The bundled directory of an installed dashboard that holds the pristine examples.
pub const EXAMPLES_SUBDIR: &str = "config";
/// Files every cluster workstation needs.
pub const ESSENTIAL: &[&str] = &["cluster.conf", "mox1.conf", "mox2.conf", "secrets.env"];
const MAX_EDIT_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigLocation {
    pub dir: PathBuf,
    /// Where the pristine `*_dot_*` examples come from. For a checkout this is
    /// the config directory itself; for an installed dashboard, its bundle.
    pub examples: PathBuf,
    /// The dashboard settings directory of an installed copy. None for a
    /// checkout, whose config directory cannot move.
    pub settings_dir: Option<PathBuf>,
    /// Why the config-location file was ignored, if it was.
    pub problem: Option<String>,
}

impl ConfigLocation {
    pub fn checkout(root: &Path) -> Self {
        let dir = root.join("config");
        Self { examples: dir.clone(), dir, settings_dir: None, problem: None }
    }

    pub fn installed(bundle_root: &Path, settings_dir: &Path) -> Self {
        let (dir, problem) = match read_pointer(settings_dir) {
            Ok(Some(dir)) => (dir, None),
            Ok(None) => (default_dir(settings_dir), None),
            Err(problem) => (default_dir(settings_dir), Some(problem)),
        };
        Self {
            dir,
            examples: bundle_root.join(EXAMPLES_SUBDIR),
            settings_dir: Some(settings_dir.to_path_buf()),
            problem,
        }
    }

    pub fn relocatable(&self) -> bool {
        self.settings_dir.is_some()
    }

    pub fn default_dir(&self) -> PathBuf {
        match &self.settings_dir {
            Some(settings) => default_dir(settings),
            None => self.dir.clone(),
        }
    }
}

pub fn default_dir(settings_dir: &Path) -> PathBuf {
    settings_dir.join("config")
}

/// Read the config-location file with the checks scripts/lib/config.sh makes.
fn read_pointer(settings_dir: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let pointer = settings_dir.join(POINTER_FILE);
    let meta = match fs::symlink_metadata(&pointer) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", pointer.display())),
    };
    let fail = |why: &str| Err(format!("{} {why}", pointer.display()));
    if !meta.file_type().is_file() || !owned_by_me(&meta) {
        return fail("must be a regular file owned by you");
    }
    if writable_by_others(&meta) {
        return fail("must not be writable by other users");
    }
    let text = fs::read_to_string(&pointer).map_err(|e| format!("{}: {e}", pointer.display()))?;
    let line = text.lines().next().unwrap_or("");
    if !line.starts_with('/') || line.contains('\r') {
        return fail("must hold one absolute directory path");
    }
    Ok(Some(PathBuf::from(line)))
}

#[cfg(unix)]
fn owned_by_me(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    meta.uid() == unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn owned_by_me(_: &fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn writable_by_others(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o022 != 0
}

#[cfg(not(unix))]
fn writable_by_others(_: &fs::Metadata) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// An operator's `*.conf`, editable in the dashboard.
    Config,
    /// A `*_dot_*` example, read-only.
    Example,
    /// A `*.env` file, whose contents the dashboard never shows.
    Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    Missing,
    /// Byte-for-byte the same as its example: nothing filled in yet.
    Unchanged,
    Customized,
    /// No example to compare with, such as mox3.conf.
    NoExample,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigFile {
    pub name: String,
    pub kind: FileKind,
    pub present: bool,
    pub essential: bool,
    /// The example this file starts from, when one is bundled.
    pub example: Option<String>,
    /// For an example, the operator file made from it.
    pub user_file: Option<String>,
    /// Config and secret files only.
    pub state: Option<FileState>,
    pub size: Option<u64>,
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigListing {
    pub dir: String,
    pub default_dir: String,
    pub relocatable: bool,
    pub problem: Option<String>,
    pub files: Vec<ConfigFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigText {
    pub name: String,
    pub text: String,
    /// Changes whenever the file's bytes change; passed back on save so a
    /// file edited elsewhere in the meantime is not overwritten.
    pub revision: String,
    pub read_only: bool,
}

pub fn classify(name: &str) -> Option<FileKind> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\', '\0']) || name.len() > 128 {
        return None;
    }
    if name.contains("_dot_") {
        Some(FileKind::Example)
    } else if name.ends_with(".env") {
        Some(FileKind::Secret)
    } else if name.ends_with(".conf") {
        Some(FileKind::Config)
    } else {
        None
    }
}

/// cluster.conf -> cluster_dot_conf.
pub fn example_for(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    Some(format!("{stem}_dot_{ext}"))
}

/// cluster_dot_conf -> cluster.conf.
pub fn user_file_for(example: &str) -> Option<String> {
    let (stem, ext) = example.rsplit_once("_dot_")?;
    Some(format!("{stem}.{ext}"))
}

fn regular_file(path: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(path).ok().filter(|m| m.file_type().is_file())
}

fn checked_name(name: &str) -> Result<FileKind> {
    classify(name).ok_or_else(|| EngineError::InvalidArgument(format!("{name} is not a BMAC config file name")))
}

/// The pristine copy of an example: the bundled one, else the one in the config directory.
fn pristine_example(loc: &ConfigLocation, example: &str) -> Option<PathBuf> {
    [loc.examples.join(example), loc.dir.join(example)].into_iter().find(|p| regular_file(p).is_some())
}

pub fn file_state(loc: &ConfigLocation, name: &str) -> FileState {
    let path = loc.dir.join(name);
    if regular_file(&path).is_none() {
        return FileState::Missing;
    }
    let Some(example) = example_for(name).and_then(|e| pristine_example(loc, &e)) else {
        return FileState::NoExample;
    };
    match (fs::read(&path), fs::read(example)) {
        (Ok(mine), Ok(theirs)) if mine == theirs => FileState::Unchanged,
        _ => FileState::Customized,
    }
}

fn modified(meta: &fs::Metadata) -> Option<String> {
    meta.modified()
        .ok()
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn describe(loc: &ConfigLocation, name: &str, kind: FileKind) -> ConfigFile {
    let meta = regular_file(&loc.dir.join(name));
    let (example, user_file, state) = match kind {
        FileKind::Example => (None, user_file_for(name), None),
        _ => (
            example_for(name).filter(|e| pristine_example(loc, e).is_some()),
            None,
            Some(file_state(loc, name)),
        ),
    };
    ConfigFile {
        name: name.to_string(),
        kind,
        present: meta.is_some(),
        essential: ESSENTIAL.contains(&name),
        example,
        user_file,
        state,
        size: meta.as_ref().map(fs::Metadata::len),
        modified: meta.as_ref().and_then(modified),
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum NameChunk {
    Number(u64),
    Text(String),
}

/// Alphabetical, except that digit runs compare by value (`mox2` before `mox10`).
fn natural_key(name: &str) -> Vec<NameChunk> {
    let mut chunks = Vec::new();
    let mut rest = name;
    while let Some(c) = rest.chars().next() {
        let digits = c.is_ascii_digit();
        let end = rest.find(|ch: char| ch.is_ascii_digit() != digits).unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        chunks.push(match run.parse() {
            Ok(n) if digits => NameChunk::Number(n),
            _ => NameChunk::Text(run.to_ascii_lowercase()),
        });
        rest = tail;
    }
    chunks
}

fn rank(file: &ConfigFile) -> (bool, Vec<NameChunk>) {
    (file.kind == FileKind::Example, natural_key(&file.name))
}

pub fn list(loc: &ConfigLocation) -> ConfigListing {
    let mut files: Vec<ConfigFile> = fs::read_dir(&loc.dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().into_string().ok()?;
            let kind = classify(&name)?;
            regular_file(&entry.path())?;
            Some(describe(loc, &name, kind))
        })
        .collect();
    for name in ESSENTIAL {
        if !files.iter().any(|f| f.name == *name) {
            files.push(describe(loc, name, classify(name).expect("essential names are config names")));
        }
    }
    files.sort_by_key(rank);
    ConfigListing {
        dir: loc.dir.display().to_string(),
        default_dir: loc.default_dir().display().to_string(),
        relocatable: loc.relocatable(),
        problem: loc.problem.clone(),
        files,
    }
}

fn revision(bytes: &[u8]) -> String {
    // FNV-1a: a change detector, not a security boundary.
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    format!("{hash:016x}-{}", bytes.len())
}

fn read_text(path: &Path, name: &str) -> Result<(String, String)> {
    let meta = regular_file(path).ok_or_else(|| EngineError::InvalidArgument(format!("{name} does not exist")))?;
    if meta.len() > MAX_EDIT_BYTES {
        return Err(EngineError::InvalidArgument(format!("{name} is too large to open here")));
    }
    let bytes = fs::read(path)?;
    let rev = revision(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| EngineError::InvalidArgument(format!("{name} is not UTF-8 text")))?;
    Ok((text, rev))
}

pub fn read(loc: &ConfigLocation, name: &str) -> Result<ConfigText> {
    let kind = checked_name(name)?;
    if kind == FileKind::Secret {
        return Err(EngineError::InvalidArgument(format!("The dashboard does not open {name}; edit it in your own editor.")));
    }
    let (text, revision) = read_text(&loc.dir.join(name), name)?;
    Ok(ConfigText { name: name.into(), text, revision, read_only: kind != FileKind::Config })
}

/// The pristine example a config file starts from, for comparing.
pub fn read_example(loc: &ConfigLocation, example: &str) -> Result<ConfigText> {
    if checked_name(example)? != FileKind::Example || example.ends_with("_dot_env") {
        return Err(EngineError::InvalidArgument(format!("{example} is not a config example")));
    }
    let path = pristine_example(loc, example).ok_or_else(|| EngineError::InvalidArgument(format!("{example} does not exist")))?;
    let (text, revision) = read_text(&path, example)?;
    Ok(ConfigText { name: example.into(), text, revision, read_only: true })
}

#[cfg(unix)]
fn set_mode(file: &fs::File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_: &fs::File, _: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn mode_of(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_: &fs::Metadata) -> u32 {
    0o600
}

/// Replace PATH atomically: write a sibling temporary file, then rename it.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let dir = path.parent().ok_or_else(|| EngineError::Io(format!("{} has no parent", path.display())))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        set_mode(&file, mode)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(Into::into)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Save an operator's `*.conf`. EXPECTED is the revision the editor loaded,
/// or None for a file the editor is creating.
pub fn write(loc: &ConfigLocation, name: &str, text: &str, expected: Option<&str>) -> Result<ConfigText> {
    if checked_name(name)? != FileKind::Config {
        return Err(EngineError::InvalidArgument(format!("{name} cannot be edited in the dashboard")));
    }
    if text.len() as u64 > MAX_EDIT_BYTES {
        return Err(EngineError::InvalidArgument(format!("{name} is too large")));
    }
    let path = loc.dir.join(name);
    let current = regular_file(&path);
    if current.is_none() && fs::symlink_metadata(&path).is_ok() {
        return Err(EngineError::InvalidArgument(format!("{name} is not a regular file")));
    }
    let current_revision = current.as_ref().map(|_| fs::read(&path).map(|b| revision(&b))).transpose()?;
    if current_revision.as_deref() != expected {
        return Err(EngineError::Unavailable(format!(
            "{name} changed on disk since it was opened. Reload it before saving."
        )));
    }
    create_private_dir(&loc.dir)?;
    write_atomic(&path, text.as_bytes(), current.as_ref().map(mode_of).unwrap_or(0o600))?;
    read(loc, name)
}

/// Create an operator file from its example. Never overwrites.
pub fn create_from_example(loc: &ConfigLocation, example: &str) -> Result<String> {
    if checked_name(example)? != FileKind::Example {
        return Err(EngineError::InvalidArgument(format!("{example} is not a config example")));
    }
    let target = user_file_for(example).filter(|t| classify(t).is_some_and(|k| k != FileKind::Example));
    let target = target.ok_or_else(|| EngineError::InvalidArgument(format!("{example} has no operator file")))?;
    let source = pristine_example(loc, example).ok_or_else(|| EngineError::InvalidArgument(format!("{example} does not exist")))?;
    let path = loc.dir.join(&target);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(EngineError::InvalidArgument(format!("{target} already exists")));
    }
    create_private_dir(&loc.dir)?;
    write_atomic(&path, &fs::read(source)?, 0o600)?;
    Ok(target)
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct SeedReport {
    pub examples_updated: Vec<String>,
    pub created: Vec<String>,
}

/// Bring an installed config directory up to date with this version's
/// bundle: replace every `*_dot_*` copy that differs, and create each missing
/// essential operator file from its example. Operator files that exist are
/// never touched.
pub fn seed(loc: &ConfigLocation) -> Result<SeedReport> {
    let mut report = SeedReport::default();
    if !loc.relocatable() || loc.examples == loc.dir {
        return Ok(report);
    }
    create_private_dir(&loc.dir)?;
    let mut examples: Vec<String> = fs::read_dir(&loc.examples)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| classify(n) == Some(FileKind::Example))
        .collect();
    examples.sort();
    for example in &examples {
        let bundled = fs::read(loc.examples.join(example))?;
        let copy = loc.dir.join(example);
        if fs::read(&copy).ok().as_deref() != Some(&bundled[..]) {
            if fs::symlink_metadata(&copy).is_ok_and(|m| !m.file_type().is_file()) {
                continue;
            }
            write_atomic(&copy, &bundled, 0o600)?;
            report.examples_updated.push(example.clone());
        }
    }
    for name in ESSENTIAL {
        let Some(example) = example_for(name).filter(|e| examples.contains(e)) else { continue };
        if fs::symlink_metadata(loc.dir.join(name)).is_err() {
            write_atomic(&loc.dir.join(name), &fs::read(loc.examples.join(example))?, 0o600)?;
            report.created.push(name.to_string());
        }
    }
    Ok(report)
}

fn move_entry(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // Across filesystems: copy, then remove the original.
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_tree(from, to)?;
            if fs::symlink_metadata(from)?.is_dir() {
                fs::remove_dir_all(from)?;
            } else {
                fs::remove_file(from)?;
            }
            Ok(())
        }
        Err(e) => Err(EngineError::Io(format!("could not move {}: {e}", from.display()))),
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else if meta.is_dir() {
        create_private_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions())?;
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

/// Point an installed dashboard at NEW_DIR, optionally moving everything in
/// the current config directory there first. Returns the new location.
pub fn relocate(loc: &ConfigLocation, new_dir: &str, move_files: bool) -> Result<ConfigLocation> {
    let settings = loc
        .settings_dir
        .as_ref()
        .ok_or_else(|| EngineError::InvalidArgument("A BMAC checkout always uses its own config directory.".into()))?;
    let new_dir = PathBuf::from(new_dir.trim_end_matches('/'));
    if !new_dir.is_absolute() || new_dir.as_os_str().is_empty() || new_dir.to_string_lossy().contains(['\n', '\r']) {
        return Err(EngineError::InvalidArgument("Choose an absolute directory path.".into()));
    }
    if fs::symlink_metadata(&new_dir).is_ok_and(|m| !m.is_dir()) {
        return Err(EngineError::InvalidArgument(format!("{} is not a directory", new_dir.display())));
    }
    let resolve = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let (old, new) = (resolve(&loc.dir), resolve(&new_dir));
    if old == new {
        return Err(EngineError::InvalidArgument("That is already the config directory.".into()));
    }
    if move_files && (new.starts_with(&old) || old.starts_with(&new)) {
        return Err(EngineError::InvalidArgument("The new directory may not contain, or be inside, the current one.".into()));
    }
    if new.starts_with(resolve(&loc.examples)) {
        return Err(EngineError::InvalidArgument("Choose a directory outside the dashboard's installation.".into()));
    }
    create_private_dir(&new_dir)?;
    if move_files && loc.dir.is_dir() {
        let entries: Vec<_> = fs::read_dir(&loc.dir)?.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        let conflicts: Vec<String> = entries
            .iter()
            .filter(|name| fs::symlink_metadata(new_dir.join(name)).is_ok())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        if !conflicts.is_empty() {
            return Err(EngineError::InvalidArgument(format!(
                "{} already contains {}. Move or remove them first, or switch without moving.",
                new_dir.display(),
                conflicts.join(", ")
            )));
        }
        for name in &entries {
            move_entry(&loc.dir.join(name), &new_dir.join(name))?;
        }
    }
    let pointer = settings.join(POINTER_FILE);
    if new == resolve(&default_dir(settings)) {
        match fs::remove_file(&pointer) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    } else {
        fs::create_dir_all(settings)?;
        write_atomic(&pointer, format!("{}\n", new_dir.display()).as_bytes(), 0o600)?;
    }
    Ok(ConfigLocation { dir: new_dir, examples: loc.examples.clone(), settings_dir: Some(settings.clone()), problem: None })
}

/// A path the frontend may open in the operator's file manager or default
/// application: the config directory itself, or a listed file in it.
pub fn openable(loc: &ConfigLocation, name: Option<&str>) -> Result<PathBuf> {
    match name {
        None => {
            create_private_dir(&loc.dir)?;
            Ok(loc.dir.clone())
        }
        Some(name) => {
            checked_name(name)?;
            let path = loc.dir.join(name);
            regular_file(&path).ok_or_else(|| EngineError::InvalidArgument(format!("{name} does not exist")))?;
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _temp: tempfile::TempDir,
        bundle: PathBuf,
        settings: PathBuf,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let bundle = temp.path().join("bundle");
        let settings = temp.path().join("settings");
        fs::create_dir_all(bundle.join(EXAMPLES_SUBDIR)).unwrap();
        for (name, text) in [
            ("cluster_dot_conf", "CLUSTER=example\n"),
            ("mox1_dot_conf", "HOST=1\n"),
            ("mox2_dot_conf", "HOST=2\n"),
            ("secrets_dot_env", "SECRET=\n"),
        ] {
            fs::write(bundle.join(EXAMPLES_SUBDIR).join(name), text).unwrap();
        }
        Fixture { _temp: temp, bundle, settings }
    }

    #[test]
    fn names_map_between_examples_and_operator_files() {
        assert_eq!(example_for("cluster.conf").as_deref(), Some("cluster_dot_conf"));
        assert_eq!(example_for("secrets.env").as_deref(), Some("secrets_dot_env"));
        assert_eq!(user_file_for("mox2_dot_conf").as_deref(), Some("mox2.conf"));
        assert_eq!(classify("secrets_dot_env"), Some(FileKind::Example));
        assert_eq!(classify("secrets.env"), Some(FileKind::Secret));
        assert_eq!(classify("mox3.conf"), Some(FileKind::Config));
        for bad in ["../cluster.conf", ".hidden.conf", "prod1_ssh.key", "settings.json", "a/b.conf", ""] {
            assert_eq!(classify(bad), None, "{bad}");
        }
    }

    #[test]
    fn seeding_refreshes_examples_and_never_touches_operator_files() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        assert_eq!(loc.dir, f.settings.join("config"));
        let report = seed(&loc).unwrap();
        assert_eq!(report.created, vec!["cluster.conf", "mox1.conf", "mox2.conf", "secrets.env"]);
        assert_eq!(report.examples_updated.len(), 4);
        assert_eq!(file_state(&loc, "cluster.conf"), FileState::Unchanged);
        assert_eq!(file_state(&loc, "secrets.env"), FileState::Unchanged);

        fs::write(loc.dir.join("cluster.conf"), "CLUSTER=mine\n").unwrap();
        fs::remove_file(loc.dir.join("mox2.conf")).unwrap();
        fs::write(f.bundle.join(EXAMPLES_SUBDIR).join("cluster_dot_conf"), "CLUSTER=v2\n").unwrap();
        let report = seed(&loc).unwrap();
        assert_eq!(report.examples_updated, vec!["cluster_dot_conf"]);
        assert_eq!(report.created, vec!["mox2.conf"]);
        assert_eq!(fs::read_to_string(loc.dir.join("cluster.conf")).unwrap(), "CLUSTER=mine\n");
        assert_eq!(fs::read_to_string(loc.dir.join("cluster_dot_conf")).unwrap(), "CLUSTER=v2\n");
        assert_eq!(file_state(&loc, "cluster.conf"), FileState::Customized);
    }

    #[test]
    fn a_checkout_is_never_seeded_or_relocated() {
        let f = fixture();
        let loc = ConfigLocation::checkout(&f.bundle);
        assert_eq!(seed(&loc).unwrap(), SeedReport::default());
        assert!(relocate(&loc, "/tmp/elsewhere", false).is_err());
        let listing = list(&loc);
        assert!(!listing.relocatable);
        let missing: Vec<_> = listing.files.iter().filter(|f| !f.present).map(|f| f.name.as_str()).collect();
        assert_eq!(missing, ESSENTIAL);
    }

    #[test]
    fn listing_is_alphabetical_with_examples_last() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        seed(&loc).unwrap();
        for name in ["mox10.conf", "mox3.conf", "alpha.env", "notes.txt", "prod1_ssh.key"] {
            fs::write(loc.dir.join(name), "x").unwrap();
        }
        let names: Vec<_> = list(&loc).files.into_iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "alpha.env", "cluster.conf", "mox1.conf", "mox2.conf", "mox3.conf", "mox10.conf", "secrets.env",
                "cluster_dot_conf", "mox1_dot_conf", "mox2_dot_conf", "secrets_dot_env",
            ]
        );
    }

    #[test]
    fn secrets_are_never_read_and_only_conf_files_are_written() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        seed(&loc).unwrap();
        assert!(read(&loc, "secrets.env").is_err());
        assert!(read_example(&loc, "secrets_dot_env").is_err());
        assert!(write(&loc, "secrets.env", "X=1", None).is_err());
        assert!(write(&loc, "cluster_dot_conf", "X=1", None).is_err());
        assert!(read(&loc, "../settings.json").is_err());
        let example = read(&loc, "mox1_dot_conf").unwrap();
        assert!(example.read_only);
    }

    #[test]
    fn saving_detects_edits_made_elsewhere() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        seed(&loc).unwrap();
        let opened = read(&loc, "cluster.conf").unwrap();
        let saved = write(&loc, "cluster.conf", "CLUSTER=edited\n", Some(&opened.revision)).unwrap();
        assert_eq!(saved.text, "CLUSTER=edited\n");
        assert!(write(&loc, "cluster.conf", "CLUSTER=stale\n", Some(&opened.revision)).is_err());
        assert!(write(&loc, "mox3.conf", "HOST=3\n", Some("x")).is_err());
        write(&loc, "mox3.conf", "HOST=3\n", None).unwrap();
        assert!(write(&loc, "mox3.conf", "HOST=3\n", None).is_err());
    }

    #[test]
    fn creating_from_an_example_never_overwrites() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        assert_eq!(create_from_example(&loc, "mox2_dot_conf").unwrap(), "mox2.conf");
        assert!(create_from_example(&loc, "mox2_dot_conf").is_err());
        assert_eq!(create_from_example(&loc, "secrets_dot_env").unwrap(), "secrets.env");
    }

    #[cfg(unix)]
    #[test]
    fn relocation_moves_everything_and_records_the_pointer() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        seed(&loc).unwrap();
        fs::create_dir_all(loc.dir.join("artifacts/hosts/mox1")).unwrap();
        fs::write(loc.dir.join("artifacts/hosts/mox1/header.bin"), "luks").unwrap();
        let target = f.settings.parent().unwrap().join("elsewhere");
        let moved = relocate(&loc, target.to_str().unwrap(), true).unwrap();
        assert_eq!(moved.dir, target);
        assert!(target.join("artifacts/hosts/mox1/header.bin").is_file());
        assert!(target.join("secrets.env").is_file());
        assert!(!loc.dir.join("cluster.conf").exists());
        let pointer = f.settings.join(POINTER_FILE);
        assert_eq!(fs::read_to_string(&pointer).unwrap(), format!("{}\n", target.display()));
        assert_eq!(fs::metadata(&pointer).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(ConfigLocation::installed(&f.bundle, &f.settings).dir, target);

        let back = relocate(&moved, loc.dir.to_str().unwrap(), true).unwrap();
        assert_eq!(back.dir, loc.dir);
        assert!(!pointer.exists());
        assert!(loc.dir.join("artifacts/hosts/mox1/header.bin").is_file());
    }

    #[test]
    fn relocation_refuses_to_overwrite_existing_files() {
        let f = fixture();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        seed(&loc).unwrap();
        let target = f.settings.parent().unwrap().join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("cluster.conf"), "theirs").unwrap();
        let error = relocate(&loc, target.to_str().unwrap(), true).unwrap_err().to_string();
        assert!(error.contains("cluster.conf"), "{error}");
        assert!(loc.dir.join("cluster.conf").is_file());
        assert!(relocate(&loc, "relative/dir", false).is_err());
        assert!(relocate(&loc, loc.dir.join("inner").to_str().unwrap(), true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_pointer_files_are_reported_and_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        fs::create_dir_all(&f.settings).unwrap();
        let pointer = f.settings.join(POINTER_FILE);
        fs::write(&pointer, "/srv/bmac\n").unwrap();
        fs::set_permissions(&pointer, fs::Permissions::from_mode(0o666)).unwrap();
        let loc = ConfigLocation::installed(&f.bundle, &f.settings);
        assert_eq!(loc.dir, f.settings.join("config"));
        assert!(loc.problem.unwrap().contains("must not be writable"));
        fs::set_permissions(&pointer, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&pointer, "relative\n").unwrap();
        assert!(ConfigLocation::installed(&f.bundle, &f.settings).problem.is_some());
    }
}
