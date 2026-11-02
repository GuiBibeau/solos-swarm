use sha2::{Digest, Sha256};
use std::fmt::Write;

/// `prefix_` followed by 16 hex digits of a length-prefixed SHA-256 of `parts`.
pub fn key(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(prefix.len() + 17);
    out.push_str(prefix);
    out.push('_');
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::key;

    #[test]
    fn equal_inputs_equal_ids_and_parts_do_not_run_together() {
        assert_eq!(key("tk", &["a", "b"]), key("tk", &["a", "b"]));
        assert_ne!(key("tk", &["ab", ""]), key("tk", &["a", "b"]));
        assert!(key("tk", &["a"]).starts_with("tk_"));
        assert_eq!(key("tk", &["a"]).len(), 3 + 16);
    }
}
