//! A device's or client's identity: an Ed25519 key made once and kept
//! (owner-only) as PKCS#8 PEM. Its ID is derived from the public key, so a
//! device keeps its ID as long as its key survives (the data partition),
//! and nothing else can claim it.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Identity {
    key: KeyPair,
}

/// Crockford's base32 alphabet: no I, L, O or U to misread.
const BASE32: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The ID of the holder of `public_key`: 80 bits of its SHA-256 in base32,
/// as `xxxx-xxxx-xxxx-xxxx`.
pub fn device_id(public_key: &[u8; 32]) -> String {
    let hash = digest::digest(&digest::SHA256, public_key);
    let bytes = &hash.as_ref()[..10];
    let mut bits: u128 = 0;
    for byte in bytes {
        bits = bits << 8 | u128::from(*byte);
    }
    let mut out = String::with_capacity(19);
    for index in 0..16 {
        if index > 0 && index % 4 == 0 {
            out.push('-');
        }
        let shift = 75 - index * 5;
        out.push(BASE32[((bits >> shift) & 31) as usize] as char);
    }
    out
}

impl Identity {
    pub fn generate() -> Result<Self> {
        let key = KeyPair::generate_for(&PKCS_ED25519).context("generating an Ed25519 key")?;
        Ok(Self { key })
    }

    pub fn from_pem(pem: &str) -> Result<Self> {
        let key = KeyPair::from_pem(pem).context("reading the identity key")?;
        if key.public_key_raw().len() != 32 || !key.is_compatible(&PKCS_ED25519) {
            bail!("the identity key is not Ed25519");
        }
        Ok(Self { key })
    }

    pub fn to_pem(&self) -> String {
        self.key.serialize_pem()
    }

    /// The identity kept in `path`, made (owner-only) when there is none.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(pem) => {
                Self::from_pem(&pem).with_context(|| format!("identity key {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                write_private(path, identity.to_pem().as_bytes())?;
                Ok(identity)
            }
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key
            .public_key_raw()
            .try_into()
            .expect("Ed25519 public keys are 32 bytes")
    }

    pub fn id(&self) -> String {
        device_id(&self.public_key())
    }

    /// A self-signed certificate for TLS. Only its key matters: the peer
    /// pins the key, not the name or dates.
    pub fn certificate(&self) -> Result<CertificateDer<'static>> {
        let params = CertificateParams::new(vec!["runo.invalid".to_owned()])
            .map_err(|err| anyhow!("certificate parameters: {err}"))?;
        let cert = params
            .self_signed(&self.key)
            .map_err(|err| anyhow!("signing the certificate: {err}"))?;
        Ok(cert.der().clone())
    }

    pub fn private_key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der()))
    }
}

/// Writes `bytes` to a new file only its owner can read.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let temp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temp)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_kept_and_its_id_follows_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys/identity.pem");
        let first = Identity::load_or_create(&path).unwrap();
        let again = Identity::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), again.public_key());
        assert_eq!(first.id(), again.id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let other = Identity::generate().unwrap();
        assert_ne!(other.id(), first.id());

        let id = first.id();
        assert_eq!(id.len(), 19);
        assert_eq!(id.matches('-').count(), 3);
        assert!(id.chars().all(|c| c == '-' || BASE32.contains(&(c as u8))));
        assert!(first.certificate().is_ok());
    }

    #[test]
    fn a_broken_key_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.pem");
        std::fs::write(&path, "not a key").unwrap();
        assert!(Identity::load_or_create(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");
    }
}
