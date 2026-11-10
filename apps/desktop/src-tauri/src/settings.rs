use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Cmd,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub gesture_enabled: bool,
    /// Require this key held during the pinch (avoids clashing with the OS's own pinch-zoom).
    pub gesture_modifier: Option<Modifier>,
    pub pinch_threshold: f64,
    pub hotkey_copy: String,
    pub hotkey_paste: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            gesture_enabled: true,
            gesture_modifier: None,
            pinch_threshold: 0.35,
            hotkey_copy: "CommandOrControl+Alt+C".into(),
            hotkey_paste: "CommandOrControl+Alt+V".into(),
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        std::fs::read(dir.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        let p: PathBuf = dir.join("settings.json");
        let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_survive_partial_and_corrupt_files() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Settings::load(d.path()), Settings::default());
        std::fs::write(d.path().join("settings.json"), r#"{"gesture_enabled": false}"#).unwrap();
        let s = Settings::load(d.path());
        assert!(!s.gesture_enabled);
        assert_eq!(s.hotkey_copy, Settings::default().hotkey_copy);
        std::fs::write(d.path().join("settings.json"), "not json").unwrap();
        assert_eq!(Settings::load(d.path()), Settings::default());
    }

    #[test]
    fn roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let s = Settings { gesture_modifier: Some(Modifier::Alt), ..Default::default() };
        s.save(d.path());
        assert_eq!(Settings::load(d.path()), s);
    }
}
