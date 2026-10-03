use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("NyaRemoteControl").join("client")
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: String,
    pub address: String,
    /// Pinned server certificate fingerprint (hex).
    #[serde(default)]
    pub fingerprint: String,
    /// Unix seconds of the last successful connection (0 = never).
    #[serde(default)]
    pub last_connected: u64,
    /// The name the host gave itself at the last connection.
    #[serde(default)]
    pub server_name: String,
    /// Named here: keep `name` instead of following `server_name`. Entries
    /// from before this field may have been renamed, so they keep theirs.
    #[serde(default = "yes")]
    pub custom_name: bool,
    /// Connection settings for this host; `None` = the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Defaults>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    /// "office" | "game"
    pub mode: String,
    pub fullscreen: bool,
    /// Display id on the host; 0 = primary.
    pub display: u32,
    /// 0 = let the host decide.
    pub bitrate_kbps: u32,
    /// Ask for the highest bitrate the encoder allows (ignores `bitrate_kbps`).
    pub unlimited_bitrate: bool,
    /// "auto" | "quality" | "balanced" | "smooth" | "fixed"
    pub bitrate_policy: String,
    /// How video travels: "auto" (datagrams + FEC in game mode) | "stream" | "datagram"
    pub video_transport: String,
    /// 0 = local monitor refresh rate.
    pub max_fps: u32,
    /// "auto" | "nvenc" | "qsv" | "amf" | "software"
    pub encoder: String,
    /// "auto" | "h264" | "hevc" | "av1"
    pub codec: String,
    /// "auto" | "420" | "444"
    pub chroma: String,
    pub audio: bool,
    pub clipboard: bool,
    /// Use hardware decoding when possible.
    pub hw_decode: bool,
    /// Virtual screens to create on the host (0 = none, up to 4).
    pub vd_count: u32,
    /// Switch the host's physical displays off (with at least one virtual screen).
    pub physical_off: bool,
    /// Block the host's own keyboard and mouse.
    pub block_input: bool,
    /// Virtual display size: "window" (follows this window) | "screen" | "fixed"
    pub vd_size: String,
    pub vd_width: u32,
    pub vd_height: u32,
    /// Give the virtual display this computer's display scaling.
    pub vd_scale: bool,
    /// Open every host display in its own window (otherwise one window, switch between them).
    pub multi_window: bool,
    /// Send this computer's microphone to the host after connecting.
    pub mic: bool,
    /// Capture the keyboard (Win key combinations go to the host) after connecting.
    pub grab_keyboard: bool,
    /// Folders of this computer shown on the host as a drive (needs WinFsp there).
    pub shared_folders: Vec<SharedFolder>,
    /// What to do with the host's print jobs: "print" (default printer) | "open" | "save"
    pub print_mode: String,
    /// HDR10 video when the host desktop and this window's monitor are HDR.
    pub hdr: bool,
    /// How the session travels: "auto" (UDP; TCP when UDP does not connect
    /// or loses too much) | "udp" | "tcp" (QUIC over TCP).
    pub transport: String,
}

/// One folder shared with the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedFolder {
    pub path: String,
    /// Directory name on the host's drive.
    pub name: String,
    pub read_only: bool,
}

impl Defaults {
    /// The shared folders that exist, with unique names.
    pub fn shares(&self) -> nya_transport::folders::Shares {
        let mut out: Vec<nya_transport::folders::Share> = Vec::new();
        for f in &self.shared_folders {
            let root = std::path::PathBuf::from(&f.path);
            if !root.is_dir() {
                tracing::warn!("shared folder {} is missing; not shared", f.path);
                continue;
            }
            let base = match f.name.trim() {
                "" => root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "共享".into()),
                n => n.replace(['/', '\\', ':'], "_"),
            };
            let mut name = base.clone();
            let mut i = 2;
            while out.iter().any(|s| s.name.eq_ignore_ascii_case(&name)) {
                name = format!("{base} ({i})");
                i += 1;
            }
            out.push(nya_transport::folders::Share { name, root, read_only: f.read_only });
        }
        nya_transport::folders::Shares(out)
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            mode: "office".into(),
            fullscreen: false,
            display: 0,
            bitrate_kbps: 0,
            unlimited_bitrate: false,
            bitrate_policy: "auto".into(),
            video_transport: "auto".into(),
            max_fps: 0,
            encoder: "auto".into(),
            codec: "auto".into(),
            chroma: "auto".into(),
            audio: true,
            clipboard: true,
            hw_decode: true,
            vd_count: 0,
            physical_off: false,
            block_input: false,
            vd_size: "window".into(),
            vd_width: 1920,
            vd_height: 1080,
            vd_scale: true,
            multi_window: false,
            mic: false,
            grab_keyboard: true,
            shared_folders: Vec::new(),
            print_mode: "print".into(),
            hdr: true,
            transport: "auto".into(),
        }
    }
}

/// Command-line overrides for one connection (`NyaRemoteControl connect … --mode game`).
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub mode: Option<String>,
    pub display: Option<u32>,
    pub fullscreen: bool,
    pub encoder: Option<String>,
    pub codec: Option<String>,
    pub chroma: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub sw_decode: bool,
}

impl Overrides {
    pub fn apply(&self, d: &mut Defaults) {
        if let Some(m) = &self.mode {
            d.mode = m.clone();
        }
        if let Some(x) = self.display {
            d.display = x;
        }
        d.fullscreen |= self.fullscreen;
        if let Some(x) = &self.encoder {
            d.encoder = x.clone();
        }
        if let Some(x) = &self.codec {
            d.codec = x.clone();
        }
        if let Some(x) = &self.chroma {
            d.chroma = x.clone();
        }
        if let Some(x) = self.bitrate_kbps {
            d.bitrate_kbps = x;
        }
        if self.sw_decode {
            d.hw_decode = false;
        }
    }
}

/// Bumped when a stored setting has to be changed once on load (see `migrate`).
const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    /// `CONFIG_VERSION` this file was last migrated to (0 = before versions).
    #[serde(default)]
    pub version: u32,
    /// This computer's name as hosts show it; empty = the computer name.
    #[serde(default)]
    pub client_name: String,
    /// Look for a new version when starting (installing is the user's choice).
    #[serde(default = "yes")]
    pub check_updates: bool,
    /// Closing the launcher keeps the program in the tray (else it quits).
    #[serde(default = "yes")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub hosts: Vec<HostEntry>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            client_name: String::new(),
            check_updates: true,
            close_to_tray: true,
            defaults: Defaults::default(),
            hosts: Vec::new(),
        }
    }
}

impl ClientConfig {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("client.toml");
        if !path.exists() {
            let c = Self::default();
            c.save(dir)?;
            return Ok(c);
        }
        let text = std::fs::read_to_string(&path)?;
        let mut c: Self = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if c.migrate() {
            if let Err(e) = c.save(dir) {
                tracing::warn!("save migrated settings: {e:#}");
            }
        }
        Ok(c)
    }

    /// One-time changes to settings stored by older versions; true if any.
    fn migrate(&mut self) -> bool {
        if self.version >= CONFIG_VERSION {
            return false;
        }
        if self.version < 1 {
            // Up to 0.7.1 keyboard capture was off by default (meant to be
            // on), so Alt+Tab & co. acted locally; nobody could tell the
            // default from a choice, so capture is switched on everywhere.
            self.defaults.grab_keyboard = true;
            for h in &mut self.hosts {
                if let Some(s) = &mut h.settings {
                    s.grab_keyboard = true;
                }
            }
        }
        self.version = CONFIG_VERSION;
        true
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("client.toml"), toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Find by name or address.
    pub fn find(&self, key: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.name == key || h.address == key)
    }

    /// The settings to connect to `address` with.
    pub fn settings_for(&self, address: &str) -> Defaults {
        self.hosts.iter().find(|h| h.address == address).and_then(|h| h.settings.clone()).unwrap_or_else(|| self.defaults.clone())
    }

    /// Change the settings of a saved host (starting from the defaults).
    pub fn edit_settings(&mut self, address: &str, f: impl FnOnce(&mut Defaults)) -> bool {
        let defaults = &self.defaults;
        let Some(h) = self.hosts.iter_mut().find(|h| h.address == address) else { return false };
        f(h.settings.get_or_insert_with(|| defaults.clone()));
        true
    }

    /// `base`, or `base (2)` … so that no host other than `except` has it.
    fn unique_name(&self, base: &str, except: Option<usize>) -> String {
        let taken = |n: &str| self.hosts.iter().enumerate().any(|(j, h)| Some(j) != except && (h.name == n || h.address == n));
        let mut name = base.to_owned();
        let mut n = 2;
        while taken(&name) {
            name = format!("{base} ({n})");
            n += 1;
        }
        name
    }

    /// What host `i` is called when it is not named here.
    fn automatic_name(&self, i: usize) -> String {
        let h = &self.hosts[i];
        let base = if h.server_name.trim().is_empty() { h.address.clone() } else { h.server_name.trim().to_owned() };
        self.unique_name(&base, Some(i))
    }

    /// Rename host `i`; an empty name goes back to the name the host gives
    /// itself. Names are how hosts are picked on the command line, so they
    /// must stay unique.
    pub fn rename(&mut self, i: usize, name: &str) -> Result<(), &'static str> {
        if i >= self.hosts.len() {
            return Err("被控端不存在");
        }
        let name = name.trim();
        if name.is_empty() {
            self.hosts[i].name = self.automatic_name(i);
            self.hosts[i].custom_name = false;
            return Ok(());
        }
        if self.hosts.iter().enumerate().any(|(j, h)| j != i && (h.name == name || h.address == name)) {
            return Err("已有同名的被控端");
        }
        let h = &mut self.hosts[i];
        h.name = name.to_owned();
        h.custom_name = true;
        Ok(())
    }

    /// Save a host added by hand; an empty name follows the host's own name
    /// once connected.
    pub fn add(&mut self, address: &str, name: &str) {
        let custom = !name.trim().is_empty();
        let base = if custom { name.trim() } else { address };
        let name = self.unique_name(base, None);
        self.hosts.push(HostEntry { name, address: address.to_owned(), custom_name: custom, ..Default::default() });
    }

    /// Record a successful connection: matched by address, added if new.
    /// Unless named here, the host is shown under the name it gives itself.
    /// Returns the host's name.
    pub fn connected(&mut self, address: &str, server_name: &str, fingerprint: String, now: u64) -> String {
        let i = match self.hosts.iter().position(|h| h.address == address) {
            Some(i) => i,
            None => {
                self.hosts.push(HostEntry { address: address.to_owned(), custom_name: false, ..Default::default() });
                self.hosts.len() - 1
            }
        };
        let h = &mut self.hosts[i];
        h.fingerprint = fingerprint;
        h.last_connected = now.max(h.last_connected);
        h.server_name = server_name.trim().to_owned();
        if !h.custom_name || h.name.is_empty() {
            self.hosts[i].name = self.automatic_name(i);
        }
        self.hosts[i].name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_host_unless_renamed() {
        let mut c = ClientConfig::default();
        assert_eq!(c.connected("10.0.0.1", "pc", "aa".into(), 1), "pc");
        assert_eq!(c.connected("10.0.0.2", "pc", "bb".into(), 1), "pc (2)");
        // The host renames itself: followed.
        assert_eq!(c.connected("10.0.0.1", "office-pc", "aa".into(), 2), "office-pc");
        // Renamed here: kept across connections.
        c.rename(0, " 公司 ").unwrap();
        assert_eq!(c.connected("10.0.0.1", "other", "ab".into(), 3), "公司");
        assert_eq!(c.hosts[0].fingerprint, "ab");
        assert!(c.rename(0, "pc (2)").is_err());
        assert!(c.rename(1, "10.0.0.1").is_err());
        // An empty name goes back to the host's own.
        c.rename(0, "  ").unwrap();
        assert_eq!((c.hosts[0].name.as_str(), c.hosts[0].custom_name), ("other", false));
        // Added by hand without a name: shows the address until connected.
        c.add("10.0.0.3", "");
        assert_eq!(c.hosts[2].name, "10.0.0.3");
        assert_eq!(c.connected("10.0.0.3", "nas", "cc".into(), 4), "nas");
        c.add("10.0.0.4", "家里");
        assert_eq!(c.connected("10.0.0.4", "nas", "dd".into(), 4), "家里");
    }

    #[test]
    fn per_host_settings() {
        let mut c = ClientConfig::default();
        c.connected("10.0.0.1", "pc", "aa".into(), 1);
        assert!(!c.settings_for("10.0.0.1").mic);
        c.defaults.mic = true;
        assert!(c.settings_for("10.0.0.1").mic, "no own settings: the defaults");
        assert!(c.edit_settings("10.0.0.1", |d| d.mode = "game".into()));
        let d = c.settings_for("10.0.0.1");
        assert_eq!((d.mode.as_str(), d.mic), ("game", true), "own settings start from the defaults");
        assert_eq!(c.settings_for("10.0.0.9").mode, "office");
        assert!(!c.edit_settings("10.0.0.9", |_| {}));
    }

    #[test]
    fn keyboard_capture_switched_on_once() {
        let text = "[defaults]
grab_keyboard = false
[[hosts]]
name = \"a\"
address = \"10.0.0.1\"
[hosts.settings]
grab_keyboard = false
";
        let mut c: ClientConfig = toml::from_str(text).unwrap();
        assert!(c.migrate());
        assert!(c.defaults.grab_keyboard && c.settings_for("10.0.0.1").grab_keyboard);
        // Switched off again afterwards: stays off.
        c.defaults.grab_keyboard = false;
        let mut c: ClientConfig = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(!c.migrate());
        assert!(!c.defaults.grab_keyboard);
        assert!(ClientConfig::default().defaults.grab_keyboard);
    }

    #[test]
    fn old_entries_keep_their_names() {
        let c: ClientConfig = toml::from_str("[[hosts]]\nname = \"公司\"\naddress = \"10.0.0.1\"\n").unwrap();
        assert!(c.hosts[0].custom_name && c.hosts[0].settings.is_none());
    }
}
