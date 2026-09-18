//! Title derivation from normalized Markdown.

use pulldown_cmark::{Event, Options, Parser};

/// First H1 heading text, else the file stem from `source_key`.
pub fn derive_title(normalized: &str, source_key: &str) -> String {
    if let Some(title) = first_h1(normalized) {
        return title;
    }
    file_stem_of(source_key).to_string()
}

fn first_h1(normalized: &str) -> Option<String> {
    let parser = Parser::new_ext(normalized, Options::ENABLE_TABLES);
    let mut in_h1 = false;
    let mut text = String::new();
    for event in parser {
        match event {
            Event::Start(pulldown_cmark::Tag::Heading {
                level: pulldown_cmark::HeadingLevel::H1,
                ..
            }) => {
                in_h1 = true;
            }
            Event::End(pulldown_cmark::TagEnd::Heading(pulldown_cmark::HeadingLevel::H1)) => {
                if in_h1 {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                    return None;
                }
            }
            Event::Text(t) if in_h1 => text.push_str(&t),
            _ => {}
        }
    }
    None
}

fn file_stem_of(source_key: &str) -> &str {
    let key = source_key
        .strip_prefix(momotaro_contracts::NOTE_SCHEME)
        .unwrap_or(source_key);
    let base = key.rsplit('/').next().unwrap_or(key);
    strip_md_extension(base)
}

/// Strips a trailing `.md`, ignoring ASCII case (the walker accepts `.MD`).
///
/// Never splits a multi-byte character: `rsplit_once` works on characters,
/// and a name with no dot is returned unchanged.
fn strip_md_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension)) if extension.eq_ignore_ascii_case("md") => stem,
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_first_h1_text() {
        let doc = "intro para\n\n# Real Title\n\nbody\n\n# Second\n";
        assert_eq!(derive_title(doc, "a/b.md"), "Real Title");
    }

    #[test]
    fn h1_with_inline_markup() {
        let doc = "# Ta *gore* 楽\n";
        assert_eq!(derive_title(doc, "x.md"), "Ta gore 楽");
    }

    #[test]
    fn falls_back_to_file_stem() {
        assert_eq!(
            derive_title("no heading here\n", "notes/ml/decay.md"),
            "decay"
        );
        assert_eq!(derive_title("", "plain"), "plain");
    }

    #[test]
    fn falls_back_to_file_stem_for_prefixed_keys() {
        assert_eq!(derive_title("no heading\n", "note:fourier.md"), "fourier");
        assert_eq!(derive_title("no heading\n", "note:ml/decay.md"), "decay");
    }

    /// The vault walker accepts `.MD` (case-insensitive); the stem strip must
    /// agree, or the title keeps a dangling extension (review suggestion).
    #[test]
    fn falls_back_to_file_stem_for_uppercase_extension() {
        assert_eq!(derive_title("no heading\n", "note:Notes/Inner.MD"), "Inner");
    }

    /// Names shorter than the extension, and a cut that would land inside a
    /// multi-byte character, fall back to the whole name without panicking
    /// (review suggestion: pin both edge branches).
    #[test]
    fn file_stem_handles_short_and_multibyte_names() {
        assert_eq!(derive_title("", "note:a"), "a");
        assert_eq!(derive_title("", "note:ab"), "ab");
        assert_eq!(derive_title("", "note:\u{65e5}"), "\u{65e5}");
        assert_eq!(derive_title("", "note:\u{1f600}"), "\u{1f600}");
        assert_eq!(derive_title("", "note:\u{65e5}.md"), "\u{65e5}");
        assert_eq!(derive_title("", "note:.MD"), "");
    }

    #[test]
    fn empty_h1_falls_back() {
        assert_eq!(derive_title("#\n\ntext\n", "doc.md"), "doc");
    }
}
