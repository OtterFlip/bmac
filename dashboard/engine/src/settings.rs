//! Native application settings, stored in the platform config directory.
//! The frontend can change them only through these typed fields.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The BMAC checkout to drive. None means "discover it".
    pub repository: Option<String>,
    /// Refresh dashboard state on this interval while it is visible; 0 is off.
    pub refresh_interval_seconds: u32,
    /// Desktop notifications when a run needs input or finishes in the background.
    pub notifications: bool,
    /// Pre-select the dry-run option for workflows that have one.
    pub default_dry_run: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { repository: None, refresh_interval_seconds: 0, notifications: true, default_dry_run: true }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp: PathBuf = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("settings serialize"))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn sanitized(mut self) -> Self {
        self.refresh_interval_seconds = match self.refresh_interval_seconds {
            0 => 0,
            n => n.clamp(60, 3600),
        };
        self
    }
}
