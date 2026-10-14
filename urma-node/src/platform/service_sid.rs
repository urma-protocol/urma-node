//! The per-service SID Windows derives from a service name: SHA-1 over the
//! upper-cased UTF-16LE name, read as five little-endian sub-authorities.
use sha1::{Digest, Sha1};

pub fn service_sid(name: &str) -> String {
    let mut hasher = Sha1::new();
    for unit in name.to_uppercase().encode_utf16() {
        hasher.update(unit.to_le_bytes());
    }
    let digest = hasher.finalize();
    let parts: Vec<String> = digest
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| u32::from_le_bytes(*chunk).to_string())
        .collect();
    format!("S-1-5-80-{}", parts.join("-"))
}
