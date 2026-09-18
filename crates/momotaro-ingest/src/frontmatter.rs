//! The `---` block at the top of a note: parsed, kept, and taken out of the body.
//!
//! The block is Obsidian's data, not ours (D41.5 in
//! `docs/docs/ingest-interop.md`): unknown fields are first-class, the block is
//! never folded back into the body, and a block that does not parse is kept
//! verbatim while the body is indexed anyway. What comes out of here goes to
//! `source_revisions.metadata_json`; only the *body* reaches the chunker, which
//! is what "正文 FTS 排除 frontmatter" means in practice.

/// A parsed `---` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// Fields in source order. A repeated key keeps every entry: reference
    /// implementations disagree on which one wins, and picking for the user
    /// would be data loss.
    pub fields: Vec<Field>,
    /// The block exactly as written, delimiter lines excluded.
    pub raw: String,
}

/// One `key: value` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The key as written.
    pub key: String,
    /// The value as written, in the closest shape the parser can name.
    pub value: Value,
}

/// A field's value, kept as close to its source as it can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `key: text`
    Scalar(String),
    /// `key:` followed by `- item` lines, in order.
    List(Vec<String>),
    /// Anything else — a bare `key:`, nested maps, block scalars, shapes this
    /// parser does not model — as the source lines, verbatim.
    Raw(String),
}

/// What a note's frontmatter turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterParse {
    /// No `---` block: the body is the whole note.
    Absent,
    /// A block that parsed.
    Parsed(Frontmatter),
    /// A block that was there but could not be read: kept verbatim, with the
    /// reason, and the body still indexed (`frontmatter_parse_error`, D41.5).
    Failed {
        /// The block's text, delimiter lines excluded, exactly as written.
        raw: String,
        /// Why it could not be read, naming the line.
        reason: String,
    },
}

/// Splits a note into its frontmatter and its body.
///
/// A block counts as frontmatter only when line 1 is exactly `---`: a note that
/// merely starts with a horizontal rule is not frontmatter, and guessing would
/// eat its first paragraph. The block ends at the first line that is exactly
/// `---` or `...`; without one there is no block at all (that `---` was a rule).
pub fn split(source: &str) -> (FrontmatterParse, &str) {
    let Some((block, body)) = take_block(source) else {
        return (FrontmatterParse::Absent, source);
    };
    match parse_block(block) {
        Ok(fields) => (
            FrontmatterParse::Parsed(Frontmatter {
                fields,
                raw: block.to_owned(),
            }),
            body,
        ),
        Err(reason) => (
            FrontmatterParse::Failed {
                raw: block.to_owned(),
                reason,
            },
            body,
        ),
    }
}

/// The block's text (delimiters excluded) and the body after it, if there is one.
fn take_block(source: &str) -> Option<(&str, &str)> {
    let rest = strip_line(source, "---")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        let bare = bare.strip_suffix('\r').unwrap_or(bare);
        if bare == "---" || bare == "..." {
            let block = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((block, body));
        }
        offset += line.len();
    }
    None
}

/// Consumes an exact first line, returning the rest.
fn strip_line<'a>(source: &'a str, expected: &str) -> Option<&'a str> {
    let rest = source.strip_prefix(expected)?;
    match rest.strip_prefix("\r\n") {
        Some(rest) => Some(rest),
        None => rest.strip_prefix('\n'),
    }
}

/// Reads the block's lines into fields, or says why it could not.
fn parse_block(block: &str) -> Result<Vec<Field>, String> {
    let mut fields: Vec<Field> = Vec::new();
    let mut list: Vec<String> = Vec::new();
    let mut raw: Vec<&str> = Vec::new();

    // Whatever is open — a list or an unmodelled block — closes here.
    fn settled(
        fields: &mut Vec<Field>,
        key: &mut Option<String>,
        list: &mut Vec<String>,
        raw: &mut Vec<&str>,
    ) {
        let Some(current) = key.take() else {
            raw.clear();
            return;
        };
        let value = if !raw.is_empty() {
            Value::Raw(raw.join("\n"))
        } else if !list.is_empty() {
            Value::List(std::mem::take(list))
        } else {
            Value::Raw(String::new())
        };
        fields.push(Field {
            key: current,
            value,
        });
        raw.clear();
    }

    let mut key: Option<String> = None;
    for (number, line) in block.split_inclusive('\n').enumerate() {
        let bare = line.strip_suffix('\n').unwrap_or(line);
        let bare = bare.strip_suffix('\r').unwrap_or(bare);
        let line_number = number + 2; // 1 is the opening delimiter.

        if bare.trim().is_empty() || bare.trim_start().starts_with('#') {
            settled(&mut fields, &mut key, &mut list, &mut raw);
            continue;
        }
        if let Some(item) = bare.trim_start().strip_prefix("- ") {
            if key.is_some() && raw.is_empty() {
                list.push(item.trim().to_owned());
                continue;
            }
            // A list item with no field to belong to is not readable.
            return Err(format!(
                "line {line_number}: a list item with no field above it"
            ));
        }
        if bare.starts_with(char::is_whitespace) {
            // Indented under something: not modelled, kept as source.
            if key.is_none() {
                return Err(format!(
                    "line {line_number}: an indented line with no field above it"
                ));
            }
            raw.push(bare);
            continue;
        }
        let Some((name, value)) = bare.split_once(':') else {
            return Err(format!("line {line_number}: not a `key: value` line"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("line {line_number}: a field with an empty name"));
        }
        settled(&mut fields, &mut key, &mut list, &mut raw);
        let value = value.trim();
        if value.is_empty() {
            key = Some(name.to_owned());
        } else {
            fields.push(Field {
                key: name.to_owned(),
                value: Value::Scalar(value.to_owned()),
            });
        }
    }
    settled(&mut fields, &mut key, &mut list, &mut raw);
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(frontmatter: &FrontmatterParse) -> &[Field] {
        match frontmatter {
            FrontmatterParse::Parsed(frontmatter) => &frontmatter.fields,
            other => panic!("expected parsed frontmatter, got {other:?}"),
        }
    }

    #[test]
    fn the_body_is_what_comes_after_the_block() {
        let source = "---\ntitle: A\n---\n\n# Body\n\ntext\n";
        let (frontmatter, body) = split(source);

        assert_eq!(
            body, "\n# Body\n\ntext\n",
            "the block is not part of the body"
        );
        assert_eq!(
            frontmatter,
            FrontmatterParse::Parsed(Frontmatter {
                fields: vec![Field {
                    key: "title".to_owned(),
                    value: Value::Scalar("A".to_owned()),
                }],
                raw: "title: A\n".to_owned(),
            })
        );
    }

    #[test]
    fn a_note_without_a_block_is_all_body() {
        let source = "# Just a note\n\n---\n\nrule, not frontmatter\n";
        let (frontmatter, body) = split(source);
        assert_eq!(frontmatter, FrontmatterParse::Absent);
        assert_eq!(body, source, "a horizontal rule is not a block");
    }

    #[test]
    fn an_unterminated_block_is_a_rule_not_frontmatter() {
        let source = "---\ntitle: A\n\n# Body\n";
        let (frontmatter, body) = split(source);
        assert_eq!(frontmatter, FrontmatterParse::Absent);
        assert_eq!(body, source, "nothing was consumed");
    }

    #[test]
    fn unknown_shapes_are_kept_as_they_were_written() {
        let source = "---\ntitle: A\naliases:\n  - one\n  - two\ncssclasses:\n  - wide\nnested:\n  deep:\n    key: value\n---\nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\n");
        let fields = fields(&frontmatter);
        let by_key = |key: &str| fields.iter().find(|field| field.key == key).expect(key);

        assert_eq!(
            by_key("aliases").value,
            Value::List(vec!["one".to_owned(), "two".to_owned()]),
            "an indented list is a list: it is how Obsidian's property editor writes one"
        );
        assert_eq!(
            by_key("cssclasses").value,
            Value::List(vec!["wide".to_owned()])
        );
        assert_eq!(
            by_key("nested").value,
            Value::Raw("  deep:\n    key: value".to_owned()),
            "a nested map is not modelled, so it is kept as source"
        );
        assert_eq!(by_key("title").value, Value::Scalar("A".to_owned()));
    }

    #[test]
    fn a_top_level_list_is_a_list_in_order() {
        let source = "---\ntags:\n- one\n- two\n---\nbody\n";
        let (frontmatter, _) = split(source);
        assert_eq!(
            fields(&frontmatter)[0].value,
            Value::List(vec!["one".to_owned(), "two".to_owned()])
        );
    }

    #[test]
    fn an_unreadable_line_fails_the_block_and_keeps_it_verbatim() {
        let source = "---\ntitle: A\nthis line has no colon\n---\nbody\n";
        let (frontmatter, body) = split(source);

        assert_eq!(
            body, "body\n",
            "the body is indexed anyway: only its frontmatter was unreadable"
        );
        match frontmatter {
            FrontmatterParse::Failed { raw, reason } => {
                assert_eq!(raw, "title: A\nthis line has no colon\n");
                assert!(reason.contains("line 3"), "it points at the line: {reason}");
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_repeated_key_keeps_every_value() {
        let source = "---\ntag: one\ntag: two\n---\nbody\n";
        let (frontmatter, _) = split(source);
        let fields = fields(&frontmatter);
        assert_eq!(fields.len(), 2, "{fields:?}");
        assert_eq!(fields[1].value, Value::Scalar("two".to_owned()));
    }

    #[test]
    fn comments_and_blank_lines_are_not_fields() {
        let source = "---\n# a comment\n\ntitle: A\n\n---\nbody\n";
        let (frontmatter, _) = split(source);
        let fields = fields(&frontmatter);
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert_eq!(fields[0].key, "title");
    }

    #[test]
    fn the_three_dot_closer_ends_the_block_too() {
        let source = "---\ntitle: A\n...\nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\n");
        assert_eq!(fields(&frontmatter)[0].key, "title");
    }

    #[test]
    fn carriage_returns_do_not_leak_into_keys() {
        let source = "---\r\ntitle: A\r\n---\r\nbody\r\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\r\n");
        let fields = fields(&frontmatter);
        assert_eq!(fields[0].key, "title", "{fields:?}");
        assert_eq!(fields[0].value, Value::Scalar("A".to_owned()));
    }
}
