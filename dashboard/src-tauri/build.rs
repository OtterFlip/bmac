//! Stages what an installed dashboard bundles beside its binary: the
//! repository's scripts/ (without tests or caches), the config `*_dot_*`
//! examples, and the bmac-installed marker that tells the scripts and the
//! dashboard they are running from a package rather than a checkout. Tauri
//! copies bundle-resources/ into the package's resource directory.

use std::fs;
use std::path::Path;

const SKIP_DIRS: &[&str] = &["tests", "__pycache__", "artifacts"];

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !SKIP_DIRS.contains(&name_str.as_ref()) {
                copy_tree(&entry.path(), &to.join(&name))?;
            }
        } else if kind.is_file() && !name_str.ends_with(".pyc") {
            fs::copy(entry.path(), to.join(&name))?;
        }
    }
    Ok(())
}

fn stage(repo: &Path, out: &Path) -> std::io::Result<()> {
    if out.exists() {
        fs::remove_dir_all(out)?;
    }
    copy_tree(&repo.join("scripts"), &out.join("scripts"))?;
    fs::create_dir_all(out.join("config"))?;
    for entry in fs::read_dir(repo.join("config"))? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().contains("_dot_") && entry.file_type()?.is_file() {
            fs::copy(entry.path(), out.join("config").join(&name))?;
        }
    }
    fs::write(
        out.join("bmac-installed"),
        "This BMAC tree was installed with the BMAC Dashboard. User files live in its config directory.\n",
    )
}

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.join("../..");
    for watched in ["scripts", "config"] {
        println!("cargo:rerun-if-changed={}", repo.join(watched).display());
    }
    stage(&repo, &manifest.join("bundle-resources")).expect("stage the bundled BMAC scripts");
    tauri_build::build()
}
