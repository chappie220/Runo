//! The pairing code (4.4): six digits both ends show, which match only if
//! nobody sits between them.
//!
//! The client commits to its nonce (sends its hash) before it sees the
//! device's, and reveals it after. The code hashes the TLS session secret
//! and both nonces, so someone in the middle, who runs two sessions with
//! different secrets, cannot pick values that make both codes agree: one
//! try in a million.

use aws_lc_rs::{constant_time, digest, rand};

pub type Nonce = [u8; 32];

pub fn nonce() -> Nonce {
    let mut out = [0u8; 32];
    rand::fill(&mut out).expect("system random numbers");
    out
}

pub fn commit(nonce: &Nonce) -> [u8; 32] {
    hash(&[b"runo pairing commit", nonce])
}

/// Whether `nonce` is the one committed to.
pub fn opens(commitment: &[u8], nonce: &Nonce) -> bool {
    constant_time::verify_slices_are_equal(commitment, &commit(nonce)).is_ok()
}

/// The six digits for a session whose secret is `session`.
pub fn code(session: &[u8; 32], device: &Nonce, client: &Nonce) -> String {
    let hash = hash(&[b"runo pairing code", session, device, client]);
    let number = u64::from_be_bytes(hash[..8].try_into().unwrap()) % 1_000_000;
    format!("{number:06}")
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    for part in parts {
        context.update(&(part.len() as u32).to_be_bytes());
        context.update(part);
    }
    context.finish().as_ref().try_into().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_depends_on_the_session_and_both_nonces() {
        let (device, client) = (nonce(), nonce());
        let session = [7u8; 32];
        let code_here = code(&session, &device, &client);
        assert_eq!(code_here.len(), 6);
        assert!(code_here.bytes().all(|b| b.is_ascii_digit()));
        assert_eq!(code(&session, &device, &client), code_here);
        // Another session or nonce gives (almost surely) another code.
        let others = [
            code(&[8u8; 32], &device, &client),
            code(&session, &nonce(), &client),
            code(&session, &device, &nonce()),
        ];
        assert!(others.iter().filter(|c| **c == code_here).count() <= 1);

        assert!(opens(&commit(&client), &client));
        assert!(!opens(&commit(&client), &device));
        assert!(!opens(b"short", &client));
    }
}
