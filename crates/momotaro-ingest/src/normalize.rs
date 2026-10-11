//! Newline/BOM normalization for source bytes.

use super::IngestError;

/// Decode bytes to UTF-8, strip a leading BOM, and normalize CRLF/CR to LF.
///
/// # Errors
///
/// - `IngestError::InvalidUtf8` when `bytes` is not valid UTF-8. The message
///   names the input as `source bytes`.
///
/// # Examples
///
/// ```
/// # use momotaro_ingest::{IngestError, normalize_markdown};
/// # fn main() -> Result<(), IngestError> {
/// let text = normalize_markdown(b"\xEF\xBB\xBFline one\r\nline two")?;
/// assert_eq!(text, "line one\nline two");
///
/// let invalid = normalize_markdown(&[0xFF, 0xFE]);
/// assert!(matches!(invalid, Err(IngestError::InvalidUtf8(_))));
/// # Ok(())
/// # }
/// ```
pub fn normalize_markdown(bytes: &[u8]) -> Result<String, IngestError> {
    let decoded = std::str::from_utf8(bytes)
        .map_err(|_| IngestError::InvalidUtf8("source bytes".to_string()))?;
    let stripped = decoded.strip_prefix('\u{feff}').unwrap_or(decoded);
    // Fast path: already LF-only.
    if !stripped.contains('\r') {
        return Ok(stripped.to_string());
    }
    let mut out = String::with_capacity(stripped.len());
    let mut chars = stripped.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_bom() {
        let bytes = [0xEF, 0xBB, 0xBF, b'#', b' ', b't'];
        assert_eq!(normalize_markdown(&bytes).unwrap(), "# t");
    }

    #[test]
    fn normalizes_crlf_and_lone_cr() {
        assert_eq!(normalize_markdown(b"a\r\nb\rc\nd").unwrap(), "a\nb\nc\nd");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            normalize_markdown("abc\n\u{4f0d}".as_bytes()).unwrap(),
            "abc\n\u{4f0d}"
        );
    }

    #[test]
    fn invalid_utf8_errors() {
        let bytes = [0xFF, 0xFE, 0x00];
        assert!(matches!(
            normalize_markdown(&bytes),
            Err(IngestError::InvalidUtf8(_))
        ));
    }
}
