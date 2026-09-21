//! The `---` block at the top of a note: parsed, kept, and taken out of the index text.
//!
//! The block is Obsidian's data, not ours (D41.5 in
//! `docs/docs/ingest-interop.md`): unknown fields are first-class, the block is
//! never folded back into the note, and a block that does not parse is kept
//! verbatim while the body is indexed anyway. What comes out of here belongs in
//! `source_revisions.metadata_json`; the *body* is what gets chunked, which is
//! what "正文 FTS 排除 frontmatter" means.
//!
//! Two things the caller has to get right, because this module cannot check them:
//!
//! - **Call this on normalised text.** The indexing path runs
//!   `normalize_markdown` first (BOM off, CRLF to LF), and a BOM in front of the
//!   opening `---` makes this function report `Absent`, which would index the
//!   block as body. Normalise first; the block then means what it looks like.
//! - **Chunk offsets stay relative to the normalised text.** The body is returned
//!   as a slice of the input, so `input.len() - body.len()` is the block's byte
//!   length: `Chunk::locator_json` addresses the *normalised* note (BOM stripped,
//!   CRLF to LF) — the same text this function is handed — so whatever chunks the
//!   body must add that offset (and the block's line count) back, or every locator
//!   in a note with frontmatter silently points into the wrong place. A consumer
//!   that slices the *file* must normalise first; `metadata_json.size_bytes` is the
//!   raw size when the two differ.

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
    /// The key as written, trimmed.
    pub key: String,
    /// The value as written, in the closest shape the parser can name.
    pub value: Value,
}

/// A field's value, kept as close to its source as it can be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `key: text` — one line, nothing continuing it.
    Scalar(String),
    /// `key:` followed by `- item` lines, in order.
    List(Vec<String>),
    /// Everything else as source lines: a bare `key:` with nothing under it
    /// (`""`), nested maps, block scalars (`|`, `>`), or a shape that mixes
    /// lists with continuations. Nothing is dropped and nothing is guessed.
    Raw(String),
}

/// Why a block could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A line that is neither a field nor a continuation of one.
    NotAField,
    /// A `- item` with no field to belong to.
    OrphanListItem,
    /// `: value` with nothing before the colon.
    EmptyFieldName,
}

impl Reason {
    /// The stable spelling written into `metadata_json.frontmatter.reason`.
    ///
    /// This is the only rendering: a reader that has to tell the three apart
    /// branches on it. The human sentence that used to sit beside it had no reader
    /// once the code was persisted, and the block's text is kept verbatim in the
    /// same blob for anyone who wants to see what the author wrote.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotAField => "not_a_field",
            Self::OrphanListItem => "orphan_list_item",
            Self::EmptyFieldName => "empty_field_name",
        }
    }
}

/// What a note's frontmatter turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterParse {
    /// No `---` block: the body is the whole note.
    Absent,
    /// A block that parsed.
    Parsed(Frontmatter),
    /// A block that was there but could not be read: kept verbatim, with the
    /// line and the reason, and the body still indexed
    /// (`frontmatter_parse_error`, D41.5).
    Failed {
        /// The block's text, delimiter lines excluded, exactly as written.
        raw: String,
        /// 1-based line where reading stopped, counting the opening `---` as
        /// line 1.
        line: usize,
        /// What was wrong with that line.
        reason: Reason,
    },
}

/// Splits a note into its frontmatter and its body.
///
/// A block counts as frontmatter only when line 1 is `---` (trailing horizontal
/// whitespace aside): a note that merely starts with a horizontal rule is not
/// frontmatter, and guessing would eat its first paragraph. The block ends at
/// the first line that is `---` or `...` under the same rule; without one there
/// is no block at all (that `---` was a rule). Only the first document is read —
/// nothing is looked ahead for a later `---`, so a second YAML document stays
/// body text. `...` is this crate's extension: YAML uses it as the document-end
/// marker, but Obsidian's own format has no such closer.
///
/// The returned body is a slice of `source`, so byte and line offsets can be
/// recovered from it — see the module documentation.
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
        Err((line, reason)) => (
            FrontmatterParse::Failed {
                raw: block.to_owned(),
                line,
                reason,
            },
            body,
        ),
    }
}

/// The block's text (delimiters excluded) and the body after it, if there is one.
fn take_block(source: &str) -> Option<(&str, &str)> {
    let rest = strip_delimiter_line(source, "---")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if is_delimiter(line, "---") || is_delimiter(line, "...") {
            let block = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((block, body));
        }
        offset += line.len();
    }
    None
}

/// Consumes an exact opening line, returning the rest.
fn strip_delimiter_line<'a>(source: &'a str, expected: &str) -> Option<&'a str> {
    let (line, rest) = first_line(source);
    if !is_delimiter(line, expected) {
        return None;
    }
    Some(rest)
}

/// The first line's content, and everything after its ending.
fn first_line(source: &str) -> (&str, &str) {
    let end = source
        .find('\n')
        .map(|index| index + 1)
        .unwrap_or(source.len());
    let (line, rest) = source.split_at(end);
    (split_ending(line).0, rest)
}

/// Whether a full line is a bare delimiter, tolerating trailing spaces and tabs.
fn is_delimiter(line: &str, expected: &str) -> bool {
    let (line, _) = split_ending(line);
    line == expected || line.trim_end_matches([' ', '\t']) == expected
}

/// Splits one line into its content and its ending (`\r\n`, `\n`, or nothing).
fn split_ending(line: &str) -> (&str, &str) {
    let mut end = line.len();
    if line.ends_with('\n') {
        end -= 1;
    }
    if line[..end].ends_with('\r') {
        end -= 1;
    }
    line.split_at(end)
}

/// What has been read of the field currently open, if any.
#[derive(Default)]
struct Pending {
    key: String,
    /// The value written on the key's own line (`key: value`).
    inline: Option<String>,
    /// `- item` lines, trimmed, in order.
    list: Vec<String>,
    /// Every line of the field as written, continuations included.
    source: Vec<String>,
    /// Set when a line went past what this parser models, which makes the whole
    /// field `Value::Raw` — nothing is dropped, and no list is half-read.
    unmodelled: bool,
}

impl Pending {
    fn settle(self) -> Field {
        let value = if self.unmodelled {
            Value::Raw(self.source.join("\n"))
        } else if !self.list.is_empty() {
            Value::List(self.list)
        } else if let Some(inline) = self.inline {
            Value::Scalar(inline)
        } else {
            Value::Raw(String::new())
        };
        Field {
            key: self.key,
            value,
        }
    }
}

/// Reads the block's lines into fields, or says which line stopped it and why.
fn parse_block(block: &str) -> Result<Vec<Field>, (usize, Reason)> {
    let mut fields: Vec<Field> = Vec::new();
    let mut pending: Option<Pending> = None;

    for (number, line) in block.split_inclusive('\n').enumerate() {
        let (bare, _) = split_ending(line);
        let line_number = number + 2; // 1 is the opening delimiter.

        if bare.trim().is_empty() || bare.trim_start().starts_with('#') {
            // Neither closes the open field: YAML allows both inside a sequence
            // and inside a block scalar, and Obsidian's properties use exactly
            // those shapes. The next `key:` line (or the end of the block) is
            // what ends a field.
            //
            // Inside a field this parser has already declared raw, they are also
            // *content* — a blank line is a line break in a literal scalar and a
            // `#` line is data, not a comment — so they are kept with the field's
            // own lines ("nothing is dropped", `Value::Raw`).
            if let Some(open) = pending.as_mut()
                && open.unmodelled
            {
                open.source.push(bare.to_owned());
            }
            continue;
        }
        if let Some(item) = bare.trim_start().strip_prefix("- ") {
            match pending.as_mut() {
                Some(open) if !open.unmodelled => {
                    open.list.push(item.trim().to_owned());
                    open.source.push(bare.to_owned());
                }
                // An item below a field that is already something else (or with
                // no field at all) is a shape this parser will not guess at.
                _ => return Err((line_number, Reason::OrphanListItem)),
            }
            continue;
        }
        if bare.starts_with(char::is_whitespace) {
            let Some(open) = pending.as_mut() else {
                return Err((line_number, Reason::NotAField));
            };
            open.source.push(bare.to_owned());
            open.unmodelled = true;
            continue;
        }
        let Some((name, value)) = bare.split_once(':') else {
            return Err((line_number, Reason::NotAField));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err((line_number, Reason::EmptyFieldName));
        }
        fields.extend(pending.take().map(Pending::settle));
        let value = value.trim();
        pending = Some(Pending {
            key: name.to_owned(),
            inline: (!value.is_empty()).then(|| value.to_owned()),
            source: vec![bare.to_owned()],
            ..Pending::default()
        });
    }
    fields.extend(pending.take().map(Pending::settle));
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

    fn field<'a>(frontmatter: &'a FrontmatterParse, key: &str) -> &'a Field {
        fields(frontmatter)
            .iter()
            .find(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field {key:?}"))
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
    fn the_block_is_measured_from_the_body() {
        // The wiring contract: body is a slice of the input, so its offset is
        // the block's byte length and every locator can stay source-relative.
        let source = "---\ntitle: A\n---\nbody\n";
        let (_, body) = split(source);
        assert_eq!(
            source.len() - body.len(),
            source.find("body").expect("body")
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
    fn a_byte_order_mark_hides_the_block() {
        // Pinned rather than fixed: the indexing path normalises first (BOM
        // off), and this test is the reason it has to.
        let source = "\u{feff}---\ntitle: A\n---\nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(frontmatter, FrontmatterParse::Absent);
        assert_eq!(body, source);
    }

    #[test]
    fn an_indented_delimiter_is_not_a_block() {
        // The first line has to be exactly `---`. Obsidian asks for the very
        // beginning of the file, and guessing would eat the first paragraph of a
        // note whose author meant an indented rule.
        let source = "  ---\ntitle: still body\n  ---\n";
        let (frontmatter, body) = split(source);
        assert!(
            matches!(frontmatter, FrontmatterParse::Absent),
            "an indented delimiter is a rule: {frontmatter:?}"
        );
        assert_eq!(
            body, source,
            "nothing is stripped when no block is recognised"
        );
    }

    #[test]
    fn a_second_document_stays_body_text() {
        // YAML allows several documents in one stream; this crate reads the first
        // and does not look ahead, so the second `---` is ordinary text. Stated as
        // a contract in `ingest-interop.md` §5.5.
        let source = "---\ntitle: one\n---\nbody\n\n---\ntitle: two\n---\n";
        let (frontmatter, body) = split(source);
        assert!(
            matches!(frontmatter, FrontmatterParse::Parsed(_)),
            "the first document is the block: {frontmatter:?}"
        );
        assert!(
            body.starts_with("body"),
            "the block ends at the first closer: {body:?}"
        );
        assert!(
            body.contains("---\ntitle: two\n---"),
            "the second document is body text, not a block: {body:?}"
        );
    }

    #[test]
    fn prose_between_two_rules_is_a_block_that_fails() {
        // The shape a reader is most likely to file as a bug: two rules with prose
        // between them. It is a block whose lines are not fields, so the block
        // fails as a whole and the body is whatever follows the closing rule.
        let source = "---\nThis is prose, not a field.\n---\nBody after.\n";
        let (frontmatter, body) = split(source);
        assert!(
            matches!(frontmatter, FrontmatterParse::Failed { .. }),
            "prose is not a field, so the block fails as a whole: {frontmatter:?}"
        );
        assert_eq!(body, "Body after.\n");
    }

    #[test]
    fn trailing_spaces_on_a_delimiter_do_not_hide_the_block() {
        let source = "--- \ntitle: A\n--- \nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\n");
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
    }

    #[test]
    fn unknown_shapes_are_kept_as_they_were_written() {
        let source = "---\ntitle: A\naliases:\n  - one\n  - two\ncssclasses:\n  - wide\nnested:\n  deep:\n    key: value\n---\nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\n");

        assert_eq!(
            field(&frontmatter, "aliases").value,
            Value::List(vec!["one".to_owned(), "two".to_owned()]),
            "an indented list is a list: it is how Obsidian's property editor writes one"
        );
        assert_eq!(
            field(&frontmatter, "cssclasses").value,
            Value::List(vec!["wide".to_owned()])
        );
        assert_eq!(
            field(&frontmatter, "nested").value,
            Value::Raw("nested:\n  deep:\n    key: value".to_owned()),
            "a nested map is not modelled, so the whole field is kept as source"
        );
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
    }

    #[test]
    fn a_block_scalar_is_kept_as_source_not_fatal() {
        // `|` and `>` are how Obsidian's property editor writes multi-line text:
        // failing the whole block over one of them would drop every other field
        // in the note (and every frontmatter link with them).
        let source = "---\ntitle: A\ndescription: |\n  line one\n  line two\nnote: >\n  folded\nstatus: draft\n---\nbody\n";
        let (frontmatter, body) = split(source);

        assert_eq!(body, "body\n");
        assert_eq!(
            field(&frontmatter, "description").value,
            Value::Raw("description: |\n  line one\n  line two".to_owned())
        );
        assert_eq!(
            field(&frontmatter, "note").value,
            Value::Raw("note: >\n  folded".to_owned())
        );
        assert_eq!(
            field(&frontmatter, "status").value,
            Value::Scalar("draft".to_owned()),
            "the fields after it are untouched"
        );
        assert_eq!(fields(&frontmatter).len(), 4);
    }

    /// Every `Reason` spelling, pinned on the value itself.
    ///
    /// The three codes are what a downstream reader branches on (5.5), so each
    /// one gets its own literal here. The previous witness was a `matches!` over
    /// the three of them, which cannot tell them apart: swapping two arms of
    /// `as_str` left the whole suite green.
    #[test]
    fn every_reason_spells_itself_the_way_the_contract_says() {
        for reason in [
            Reason::NotAField,
            Reason::OrphanListItem,
            Reason::EmptyFieldName,
        ] {
            // No `_` arm: a fourth variant fails to compile right here, which is
            // what keeps this table in step with the enum.
            let expected = match reason {
                Reason::NotAField => "not_a_field",
                Reason::OrphanListItem => "orphan_list_item",
                Reason::EmptyFieldName => "empty_field_name",
            };
            assert_eq!(reason.as_str(), expected, "spelling of {reason:?}");
        }
    }

    #[test]
    fn a_list_does_not_leak_into_the_next_field() {
        // The shape that made this parser wrong: a field whose content stops
        // being a list, followed by another list.
        let source = "---\na:\n  - one\n  x: y\nb:\n  - two\n---\nbody\n";
        let (frontmatter, _) = split(source);

        assert_eq!(
            field(&frontmatter, "a").value,
            Value::Raw("a:\n  - one\n  x: y".to_owned()),
            "the field keeps its own lines"
        );
        assert_eq!(
            field(&frontmatter, "b").value,
            Value::List(vec!["two".to_owned()]),
            "and the next field does not inherit them"
        );
    }

    #[test]
    fn a_bare_key_is_the_empty_raw_value() {
        let source = "---\nempty:\ntitle: A\n---\nbody\n";
        let (frontmatter, _) = split(source);
        assert_eq!(
            field(&frontmatter, "empty").value,
            Value::Raw(String::new())
        );
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
    }

    #[test]
    fn a_top_level_list_is_a_list_in_order() {
        let source = "---\ntags:\n- one\n- two\n---\nbody\n";
        let (frontmatter, _) = split(source);
        assert_eq!(
            field(&frontmatter, "tags").value,
            Value::List(vec!["one".to_owned(), "two".to_owned()])
        );
    }

    #[test]
    fn keys_and_values_are_trimmed_but_kept_as_text() {
        let source = "---\ntitle :   A  \nitems:\n-  spaced\n---\nbody\n";
        let (frontmatter, _) = split(source);
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
        assert_eq!(
            field(&frontmatter, "items").value,
            Value::List(vec!["spaced".to_owned()]),
            "the item's own padding is not part of the item"
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
            FrontmatterParse::Failed { raw, line, reason } => {
                assert_eq!(raw, "title: A\nthis line has no colon\n");
                assert_eq!(line, 3, "the line is a value, not a string to match on");
                assert_eq!(reason, Reason::NotAField);
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn the_other_two_reasons_are_named_as_such() {
        let (frontmatter, _) = split("---\n- orphan\n---\nbody\n");
        match frontmatter {
            FrontmatterParse::Failed { line, reason, .. } => {
                assert_eq!(line, 2);
                assert_eq!(reason, Reason::OrphanListItem);
            }
            other => panic!("expected a failure, got {other:?}"),
        }

        let (frontmatter, _) = split("---\ntitle: A\n: value\n---\nbody\n");
        match frontmatter {
            FrontmatterParse::Failed { line, reason, .. } => {
                assert_eq!(line, 3);
                assert_eq!(reason, Reason::EmptyFieldName);
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
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
        assert_eq!(fields(&frontmatter).len(), 1);
    }

    #[test]
    fn a_comment_or_a_blank_line_does_not_close_a_field() {
        let source = "---\ndescription: |\n  line one\n# a comment\n  line two\n\ntags:\n- one\n\n- two\n---\nbody\n";
        let (frontmatter, _) = split(source);

        assert_eq!(
            field(&frontmatter, "description").value,
            Value::Raw("description: |\n  line one\n# a comment\n  line two\n".to_owned()),
            "a comment inside a field is not the field's end, nor a dropped line"
        );
        assert_eq!(
            field(&frontmatter, "tags").value,
            Value::List(vec!["one".to_owned(), "two".to_owned()]),
            "a blank line inside a list is not the list's end either"
        );
        assert_eq!(fields(&frontmatter).len(), 2);
    }

    #[test]
    fn the_three_dot_closer_ends_the_block_too() {
        let source = "---\ntitle: A\n...\nbody\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\n");
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
    }

    #[test]
    fn carriage_returns_do_not_leak_into_keys_or_bodies() {
        let source = "---\r\ntitle: A\r\n---\r\nbody\r\n";
        let (frontmatter, body) = split(source);
        assert_eq!(body, "body\r\n");
        assert_eq!(
            field(&frontmatter, "title").value,
            Value::Scalar("A".to_owned())
        );
    }
}
