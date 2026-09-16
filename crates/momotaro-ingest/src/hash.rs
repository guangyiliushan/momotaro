//! Domain-separated SHA-256 helpers for revisions and chunks.

use sha2::{Digest, Sha256};

const REVISION_DOMAIN: &[u8] = b"momotaro-revision-v1\0";
const CHUNK_ID_DOMAIN: &[u8] = b"momotaro-chunk-id-v1\0";
const CHUNK_HASH_DOMAIN: &[u8] = b"momotaro-chunk-hash-v1\0";

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let digest = hasher.finalize();
    hex::encode(digest)
}

/// `hex(sha256("momotaro-revision-v1\0" + normalized))`.
pub fn revision_hash(normalized: &str) -> String {
    sha256_hex(&[REVISION_DOMAIN, normalized.as_bytes()])
}

/// `hex(sha256("momotaro-chunk-id-v1\0" + source_key + "\0" + revision_hash + "\0" + ordinal))`.
pub fn chunk_id(source_key: &str, revision_hash: &str, ordinal: u32) -> String {
    sha256_hex(&[
        CHUNK_ID_DOMAIN,
        source_key.as_bytes(),
        b"\0",
        revision_hash.as_bytes(),
        b"\0",
        ordinal.to_string().as_bytes(),
    ])
}

/// `hex(sha256("momotaro-chunk-hash-v1\0" + chunk_id + "\0" + text))`.
pub fn chunk_hash(chunk_id: &str, text: &str) -> String {
    sha256_hex(&[
        CHUNK_HASH_DOMAIN,
        chunk_id.as_bytes(),
        b"\0",
        text.as_bytes(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_64_lower_hex(s: &str) -> bool {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn revision_hash_is_stable() {
        let a = revision_hash("# hi\n");
        let b = revision_hash("# hi\n");
        assert_eq!(a, b);
        assert!(is_64_lower_hex(&a));
    }

    #[test]
    fn chunk_id_changes_with_ordinal() {
        let rh = revision_hash("x");
        assert_ne!(chunk_id("a.md", &rh, 0), chunk_id("a.md", &rh, 1));
        assert_ne!(chunk_id("a.md", &rh, 0), chunk_id("b.md", &rh, 0));
    }

    #[test]
    fn chunk_hash_changes_with_text() {
        let cid = chunk_id("a.md", &revision_hash("x"), 0);
        let h1 = chunk_hash(&cid, "one");
        let h2 = chunk_hash(&cid, "two");
        assert_ne!(h1, h2);
        assert_eq!(h1, chunk_hash(&cid, "one"));
        assert!(is_64_lower_hex(&h1));
    }
}
