use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// `server.toml`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// UDP port.
    pub port: u16,
    /// Address to bind; `::` listens on all IPv4 and IPv6 addresses. Set it to the
    /// overlay network address (e.g. Tailscale 100.x.y.z) to only accept connections there.
    pub bind: String,
    /// Name shown to clients; empty = computer name.
    pub name: String,
    /// "auto" | "nvenc" | "qsv" | "amf" | "software"
    pub encoder: String,
    /// Default bitrate in kbit/s when the client doesn't ask for one; 0 = automatic.
    pub office_bitrate_kbps: u32,
    pub game_bitrate_kbps: u32,
    pub max_fps: u32,
    pub audio: bool,
    /// tracing filter, e.g. "info" or "nya_server=debug"
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: nya_proto::DEFAULT_PORT,
            bind: "::".into(),
            name: String::new(),
            encoder: "auto".into(),
            office_bitrate_kbps: 0,
            game_bitrate_kbps: 0,
            max_fps: 144,
            audio: true,
            log_level: "info".into(),
        }
    }
}

impl ServerConfig {
    /// Load `server.toml`, writing the defaults if it doesn't exist.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join("server.toml");
        if path.exists() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            return toml::from_str(&text).with_context(|| format!("parse {}", path.display()));
        }
        let cfg = Self::default();
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            crate::winutil::computer_name()
        } else {
            self.name.clone()
        }
    }
}
