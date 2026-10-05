use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;

use crate::keymap::{ConfigFile, YAML_HEADER};

#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn discover() -> Self {
        let path = ProjectDirs::from("com", "Azrl", "scrcpy-pad-hdc")
            .map(|dirs| dirs.config_dir().join("profile.yaml"))
            .unwrap_or_else(|| PathBuf::from("profile.yaml"));
        Self { path }
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_or_default(&self) -> ConfigFile {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            let mut config = ConfigFile::default();
            config.normalize();
            return config;
        };
        match serde_norway::from_str::<ConfigFile>(&text) {
            Ok(mut config) => {
                config.normalize();
                config
            }
            Err(error) => {
                eprintln!(
                    "[config] failed to parse {}: {error:#}",
                    self.path.display()
                );
                let mut config = ConfigFile::default();
                config.normalize();
                config
            }
        }
    }

    pub fn save(&self, config: &ConfigFile) -> Result<()> {
        let text = format!("{YAML_HEADER}{}", serde_norway::to_string(config)?);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config directory {}", parent.display()))?;
        }
        std::fs::write(&self.path, text)
            .with_context(|| format!("write config {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        let path =
            std::env::temp_dir().join(format!("scrcpy-pad-hdc-config-{}.yaml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = ProfileStore::with_path(&path);
        let mut config = ConfigFile::default();
        config.schemes[0].name = "测试配置".to_string();
        store.save(&config).unwrap();
        let loaded = store.load_or_default();
        assert_eq!(loaded.active_profile().unwrap().name, "测试配置");
        let _ = std::fs::remove_file(path);
    }
}
