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

/// `hex(sha256(bytes))` — the one-buffer form of the house digest.
///
/// `sha256_hex` is domain-separated and takes the parts; a caller that has a
/// single buffer should not have to build a one-element array for it, and
/// should not get to write the `Sha256` dance a second time somewhere else.
pub fn sha256_of(bytes: &[u8]) -> String {
    sha256_hex(&[bytes])
}

/// `hex(sha256("momotaro-revision-v1\0" + normalized))`.
pub fn revision_hash(normalized: &str) -> String {
    sha256_hex(&[REVISION_DOMAIN, normalized.as_bytes()])
}

/// [`revision_hash`] for content that is not text — an attachment's bytes.
///
/// Same domain as the text form: a revision is a revision, whichever kind of
/// source it belongs to. Binary content never goes through `normalize_markdown`,
/// so this hashes exactly what the file holds, and it streams: a vault may hold
/// a video, and reading one into memory to hash it would be the whole file.
pub fn revision_hash_reader(reader: &mut impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
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
    #[test]
    fn the_streaming_hash_is_the_text_hash_for_the_same_bytes() {
        // One domain, two entry points: a revision is a revision, and an
        // attachment hashes its bytes through the reader that never holds them
        // all. If these two ever disagree, the same content would get two
        // identities depending on which kind of source it came from.
        let text = "# note\n\nbody\n";
        assert_eq!(
            revision_hash_reader(&mut std::io::Cursor::new(text.as_bytes())).expect("hash"),
            revision_hash(text)
        );
        // Content that is not text at all still hashes — that is the point.
        let binary = [0x89u8, b'P', b'N', b'G', 0xff];
        let streamed =
            revision_hash_reader(&mut std::io::Cursor::new(binary.as_slice())).expect("hash");
        assert_eq!(streamed.len(), 64, "sha256, hex");
    }
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

    #[test]
    fn the_identity_domains_are_pinned() {
        // The domains are frozen identity: re-spelling one re-identifies every
        // revision and chunk in every vault and breaks every citation
        // five-tuple, while every test above stays green — both sides of each
        // comparison move together. Literal digests are the only anchor that
        // cannot drift with the code under test.
        assert_eq!(
            revision_hash("abc"),
            "2876c774fe6e0f91739bdd2bfac9f6a5600268458b89bea9cdbef79c6e4eea4f"
        );
        assert_eq!(
            revision_hash(""),
            "95f1d4c541f4f64a3ff611404ddcf93c6d5abad310059f5d8ae9d51e9262fc32"
        );
        assert_eq!(
            chunk_id("note:a.md", "h1", 0),
            "95f12707c019c5435dbaebba6c5058da213d755ffab249896305afd6d7ade7a8"
        );
        assert_eq!(
            chunk_hash(
                "95f12707c019c5435dbaebba6c5058da213d755ffab249896305afd6d7ade7a8",
                "text",
            ),
            "d8879b76c6b7cf2b0374a9a716e8872cf0778f6d3f3803527a463d0ded326df3"
        );
    }
}
