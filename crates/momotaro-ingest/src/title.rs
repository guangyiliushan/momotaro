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
    let base = source_key.rsplit('/').next().unwrap_or(source_key);
    base.strip_suffix(".md").unwrap_or(base)
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
    fn empty_h1_falls_back() {
        assert_eq!(derive_title("#\n\ntext\n", "doc.md"), "doc");
    }
}
