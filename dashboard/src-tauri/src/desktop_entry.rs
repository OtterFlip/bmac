//! Linux desktop integration for builds that were not installed from a
//! package. GNOME and other freedesktop shells find a window's app icon
//! (Alt+Tab, dock, overview) through a `.desktop` file whose
//! `StartupWMClass` matches the window's WM_CLASS / Wayland app id, which GTK
//! derives from the executable name. `pnpm app:dev` and a bare
//! `target/release` binary have no such file, so the shell shows a generic
//! icon. This writes a hidden (`NoDisplay`) user-level entry and icon so the
//! window is matched to the BMAC icon. Packaged installs under /usr ship
//! their own entry and are left alone.

use std::path::{Path, PathBuf};

const WM_CLASS: &str = "bmac-dashboard";
const ICON: &[u8] = include_bytes!("../icons/128x128@2x.png");

pub fn register() {
    let Ok(exe) = std::env::current_exe() else { return };
    if exe.starts_with("/usr") || exe.starts_with("/opt") {
        return;
    }
    let Some(data) = data_home() else { return };
    let icon_path = data.join("icons/hicolor/256x256/apps").join(format!("{WM_CLASS}.png"));
    let entry_path = data.join("applications").join(format!("{WM_CLASS}.desktop"));
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=BMAC Dashboard\n\
         Comment=Control panel for BMAC Proxmox clusters\n\
         Exec={}\n\
         Icon={}\n\
         StartupWMClass={WM_CLASS}\n\
         Terminal=false\n\
         NoDisplay=true\n\
         Categories=Development;\n",
        exec_quote(&exe),
        icon_path.display(),
    );
    if let Err(error) = write_if_changed(&icon_path, ICON).and_then(|_| write_if_changed(&entry_path, entry.as_bytes())) {
        eprintln!("bmac-dashboard: could not register the desktop icon: {error}");
    }
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

fn write_if_changed(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if std::fs::read(path).is_ok_and(|current| current == contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Quote a path for the desktop entry `Exec` key.
fn exec_quote(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.display().to_string().chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_exec_paths() {
        assert_eq!(exec_quote(Path::new("/a b/c")), "\"/a b/c\"");
        assert_eq!(exec_quote(Path::new("/x$\"%y")), "\"/x\\$\\\"%%y\"");
    }
}
