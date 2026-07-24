//! Foundations for the frozen apply-patch description evaluation.

pub mod analysis;
pub mod artifacts;
pub mod descriptions;
pub mod fixtures;
pub mod rng;
pub mod schedule;
pub mod snapshot;
pub mod statistics;
pub mod suite;

use sha2::{Digest, Sha256};

/// Returns lowercase hexadecimal SHA-256 for `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn hash_framed(domain: &[u8], fields: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    frame(&mut hasher, domain);
    for field in fields {
        frame(&mut hasher, field);
    }
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(
        u64::try_from(bytes.len())
            .expect("byte slice length fits u64")
            .to_be_bytes(),
    );
    hasher.update(bytes);
}
