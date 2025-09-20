use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Session state that persists between runs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub last_used_model: Option<String>,
    pub last_project_path: Option<String>,
    pub operation_mode: Option<String>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            last_used_model: None,
            last_project_path: None,
            operation_mode: None,
        }
    }
}

impl SessionState {
    /// Get the path to the session file
    fn session_file() -> Result<PathBuf> {
        let home = std::env::var("HOME")?;
        let config_dir = PathBuf::from(home).join(".config").join("mermaid");
        fs::create_dir_all(&config_dir)?;
        Ok(config_dir.join("session.toml"))
    }

    /// Load session state from disk
    pub fn load() -> Result<Self> {
        let path = Self::session_file()?;
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            Ok(toml::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    /// Save session state to disk
    pub fn save(&self) -> Result<()> {
        let path = Self::session_file()?;
        let content = toml::to_string_pretty(&self)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Update the last used model
    pub fn set_model(&mut self, model: String) {
        self.last_used_model = Some(model);
    }

    /// Get the last used model
    pub fn get_model(&self) -> Option<&str> {
        self.last_used_model.as_deref()
    }
}