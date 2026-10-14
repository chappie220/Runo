//! Who may use the device (4.4): the trusted clients, the device password
//! that pairing asks for, and the pairing window.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aws_lc_rs::{pbkdf2, rand};
use runo_link::{device_id, hex};
use serde::{Deserialize, Serialize};

const CLIENTS: &str = "clients.json";
const PASSWORD: &str = "password";
const ITERATIONS: u32 = 200_000;
pub const MIN_PASSWORD: usize = 8;
/// How long the pairing window stays open.
pub const WINDOW_SECS: i64 = 5 * 60;
/// Wrong passwords before the window closes.
pub const MAX_FAILURES: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    /// The client's public key, hex.
    pub key: String,
    /// Unix time.
    pub added: i64,
    #[serde(default)]
    pub last_seen: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ClientFile {
    clients: Vec<Client>,
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The trust kept in the daemon's directory.
pub struct Trust {
    dir: PathBuf,
}

impl Trust {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn clients_path(&self) -> PathBuf {
        self.dir.join(CLIENTS)
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        match std::fs::read_to_string(self.clients_path()) {
            Ok(text) => Ok(serde_json::from_str::<ClientFile>(&text)
                .with_context(|| format!("reading {}", self.clients_path().display()))?
                .clients),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
            Err(err) => {
                Err(err).with_context(|| format!("reading {}", self.clients_path().display()))
            }
        }
    }

    fn save(&self, clients: Vec<Client>) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_vec_pretty(&ClientFile { clients })?;
        runo_link::identity::write_private(&self.clients_path(), &json)
    }

    /// The trusted client holding `key`.
    pub fn client(&self, key: &[u8; 32]) -> Option<Client> {
        let key = hex::encode(key);
        self.clients().ok()?.into_iter().find(|c| c.key == key)
    }

    /// Trusts `key` as `name`; the client's entry.
    pub fn add(&self, key: &[u8; 32], name: &str) -> Result<Client> {
        let mut clients = self.clients()?;
        let id = device_id(key);
        clients.retain(|c| c.id != id);
        let name: String = name
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect();
        let client = Client {
            id,
            name: if name.is_empty() {
                "client".into()
            } else {
                name
            },
            key: hex::encode(key),
            added: now(),
            last_seen: Some(now()),
        };
        clients.push(client.clone());
        self.save(clients)?;
        Ok(client)
    }

    /// Takes away the trust of the client `id` names (its ID or the start
    /// of it); the client removed.
    pub fn revoke(&self, id: &str) -> Result<Client> {
        let mut clients = self.clients()?;
        let id = id.trim().to_lowercase();
        let found: Vec<usize> = clients
            .iter()
            .enumerate()
            .filter(|(_, c)| c.id == id || (id.len() >= 4 && c.id.starts_with(&id)))
            .map(|(i, _)| i)
            .collect();
        let index = match found.as_slice() {
            [one] => *one,
            [] => bail!("no trusted client {id:?}"),
            _ => bail!("{id:?} names more than one client: give the whole ID"),
        };
        let client = clients.remove(index);
        self.save(clients)?;
        Ok(client)
    }

    pub fn seen(&self, id: &str) {
        if let Ok(mut clients) = self.clients()
            && let Some(client) = clients.iter_mut().find(|c| c.id == id)
        {
            client.last_seen = Some(now());
            let _ = self.save(clients);
        }
    }

    fn password_path(&self) -> PathBuf {
        self.dir.join(PASSWORD)
    }

    pub fn has_password(&self) -> bool {
        self.password_path().exists()
    }

    pub fn set_password(&self, password: &str) -> Result<()> {
        if password.chars().count() < MIN_PASSWORD {
            bail!("the device password needs at least {MIN_PASSWORD} characters");
        }
        let mut salt = [0u8; 16];
        rand::fill(&mut salt).map_err(|_| anyhow::anyhow!("no random numbers"))?;
        let mut hash = [0u8; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(ITERATIONS).unwrap(),
            &salt,
            password.as_bytes(),
            &mut hash,
        );
        let line = format!(
            "pbkdf2-sha256${ITERATIONS}${}${}\n",
            hex::encode(&salt),
            hex::encode(&hash)
        );
        std::fs::create_dir_all(&self.dir)?;
        runo_link::identity::write_private(&self.password_path(), line.as_bytes())
    }

    /// Whether `password` is the device password; never with none set.
    pub fn check_password(&self, password: &str) -> bool {
        let Ok(line) = std::fs::read_to_string(self.password_path()) else {
            return false;
        };
        let parts: Vec<&str> = line.trim().split('$').collect();
        let [scheme, iterations, salt, hash] = parts.as_slice() else {
            return false;
        };
        let (Some(iterations), Some(salt), Some(hash)) = (
            iterations.parse().ok().and_then(NonZeroU32::new),
            hex::decode(salt),
            hex::decode(hash),
        ) else {
            return false;
        };
        *scheme == "pbkdf2-sha256"
            && pbkdf2::verify(
                pbkdf2::PBKDF2_HMAC_SHA256,
                iterations,
                &salt,
                password.as_bytes(),
                &hash,
            )
            .is_ok()
    }
}

/// The pairing window: shut unless opened, for a few minutes, until one
/// client pairs or too many passwords are wrong.
#[derive(Debug, Default)]
pub struct Window {
    until: Option<i64>,
    failures: u32,
}

impl Window {
    pub fn open(&mut self, at: i64) -> i64 {
        let until = at + WINDOW_SECS;
        self.until = Some(until);
        self.failures = 0;
        until
    }

    pub fn close(&mut self) {
        self.until = None;
    }

    /// Until when it is open at `at`.
    pub fn until(&self, at: i64) -> Option<i64> {
        self.until.filter(|until| at < *until)
    }

    /// Whether its time ran out at `at` without anyone closing it.
    pub fn expired(&self, at: i64) -> bool {
        self.until.is_some_and(|until| at >= until)
    }

    #[cfg(test)]
    pub fn is_open(&self, at: i64) -> bool {
        self.until(at).is_some()
    }

    /// A wrong password; whether the window closed because of it.
    pub fn failed(&mut self) -> bool {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.close();
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clients_are_trusted_found_by_key_and_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let trust = Trust::new(dir.path());
        assert!(trust.clients().unwrap().is_empty());
        let key = [1u8; 32];
        let client = trust.add(&key, "  laptop\u{7}  ").unwrap();
        assert_eq!(client.name, "laptop");
        assert_eq!(client.id, device_id(&key));
        assert_eq!(trust.client(&key).unwrap().id, client.id);
        assert!(trust.client(&[2u8; 32]).is_none());
        trust.add(&[2u8; 32], "phone").unwrap();
        // Pairing again replaces the entry.
        trust.add(&key, "laptop 2").unwrap();
        assert_eq!(trust.clients().unwrap().len(), 2);

        assert!(trust.revoke("zz").is_err());
        let removed = trust.revoke(&client.id[..9]).unwrap();
        assert_eq!(removed.name, "laptop 2");
        assert!(trust.client(&key).is_none());
        assert_eq!(trust.clients().unwrap().len(), 1);
    }

    #[test]
    fn the_password_is_kept_hashed_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let trust = Trust::new(dir.path());
        assert!(!trust.has_password());
        assert!(!trust.check_password(""));
        assert!(trust.set_password("short").is_err());
        trust.set_password("correct horse").unwrap();
        assert!(trust.has_password());
        let kept = std::fs::read_to_string(dir.path().join(PASSWORD)).unwrap();
        assert!(!kept.contains("correct"));
        assert!(trust.check_password("correct horse"));
        assert!(!trust.check_password("correct horsE"));
    }

    #[test]
    fn the_window_closes_by_time_and_after_wrong_passwords() {
        let mut window = Window::default();
        assert!(!window.is_open(100));
        let until = window.open(100);
        assert_eq!(until, 100 + WINDOW_SECS);
        assert!(window.is_open(until - 1));
        assert!(!window.is_open(until));
        assert!(!window.expired(until - 1));
        assert!(window.expired(until));
        window.close();
        assert!(!window.expired(until));
        window.open(100);
        for _ in 1..MAX_FAILURES {
            assert!(!window.failed());
        }
        assert!(window.failed());
        assert!(!window.is_open(101));
        window.open(200);
        assert!(!window.failed());
    }
}
