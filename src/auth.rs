//! Pairing key and the list of authorised client certificates.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nya_transport::pairing::PairingKey;
use nya_transport::Fingerprint;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    pub fingerprint: String,
    pub name: String,
    pub paired_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ClientsFile {
    #[serde(default)]
    clients: Vec<PairedClient>,
}

pub struct AuthStore {
    dir: PathBuf,
    key: PairingKey,
    clients: Mutex<Vec<PairedClient>>,
    failures: Mutex<Vec<Instant>>,
}

const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);

fn now_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

pub fn load_or_create_key(dir: &Path, reset: bool) -> Result<PairingKey> {
    let path = dir.join("pairing.key");
    if !reset {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(k) = PairingKey::from_hex(&text) {
                return Ok(k);
            }
        }
    }
    std::fs::create_dir_all(dir)?;
    let k = PairingKey::generate();
    std::fs::write(&path, k.to_hex()).context("write pairing.key")?;
    Ok(k)
}

impl AuthStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let key = load_or_create_key(dir, false)?;
        let clients = Self::read_clients(dir);
        Ok(Self { dir: dir.to_owned(), key, clients: Mutex::new(clients), failures: Mutex::new(Vec::new()) })
    }

    fn read_clients(dir: &Path) -> Vec<PairedClient> {
        std::fs::read_to_string(dir.join("clients.toml"))
            .ok()
            .and_then(|t| toml::from_str::<ClientsFile>(&t).ok())
            .map(|f| f.clients)
            .unwrap_or_default()
    }

    pub fn list(dir: &Path) -> Vec<PairedClient> {
        Self::read_clients(dir)
    }

    pub fn save_list(dir: &Path, clients: Vec<PairedClient>) -> Result<()> {
        let text = toml::to_string_pretty(&ClientsFile { clients })?;
        std::fs::write(dir.join("clients.toml"), text).context("write clients.toml")
    }

    pub fn key(&self) -> &PairingKey {
        &self.key
    }

    pub fn is_paired(&self, fp: &Fingerprint) -> bool {
        // Re-read so `nya-server clients --remove` takes effect without a restart.
        let fresh = Self::read_clients(&self.dir);
        let hex = fp.to_hex();
        let ok = fresh.iter().any(|c| c.fingerprint == hex);
        *self.clients.lock().unwrap() = fresh;
        ok
    }

    pub fn add(&self, fp: &Fingerprint, name: &str) -> Result<()> {
        let mut list = self.clients.lock().unwrap();
        let hex = fp.to_hex();
        list.retain(|c| c.fingerprint != hex);
        list.push(PairedClient { fingerprint: hex, name: name.to_owned(), paired_at: now_string() });
        Self::save_list(&self.dir, list.clone())
    }

    /// Too many wrong pairing attempts recently?
    pub fn locked_out(&self) -> bool {
        let mut f = self.failures.lock().unwrap();
        f.retain(|t| t.elapsed() < FAILURE_WINDOW);
        f.len() >= MAX_FAILURES
    }

    pub fn record_failure(&self) {
        self.failures.lock().unwrap().push(Instant::now());
    }
}
