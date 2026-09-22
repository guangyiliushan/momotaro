//! Deterministic Markdown chunking driven by `IndexPolicy`.
//!
//! Blocks are folded from pulldown-cmark's offset iterator (a container's
//! `End` event carries the full container byte span, from the first byte of
//! its opening line through the last byte of its content or closing fence).
//! Chunk text is always the verbatim source slice; packed blocks are joined
//! with the literal separator `"\n\n"` and events are never re-serialized.

use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use momotaro_contracts::{Chunk, IndexPolicy, estimate_tokens};

use super::hash;

/// One foldable source block: a byte span plus the heading outline above it.
#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    range: Range<usize>,
    heading: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Code,
    Table,
    List,
}

impl BlockKind {
    fn is_text_kind(self) -> bool {
        matches!(self, BlockKind::Paragraph | BlockKind::List)
    }
}

/// Fold parsed events into ordered blocks, snapshotting the h1-h6 heading
/// stack at each block. Headings are boundaries only; their text feeds the
/// stack for subsequent blocks. A block that intersects an excluded span is
/// dropped whole: the text itself is never rewritten, so no locator moves.
fn collect_blocks(normalized: &str, excluded: &[Range<usize>]) -> Vec<Block> {
    let parser = Parser::new_ext(
        normalized,
        Options::ENABLE_TABLES | Options::ENABLE_WIKILINKS,
    );
    let mut blocks: Vec<Block> = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut heading_text = String::new();
    let mut in_heading = false;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let lvl = level as u8;
                while stack.last().is_some_and(|(l, _)| *l >= lvl) {
                    stack.pop();
                }
                stack.push((lvl, String::new()));
                in_heading = true;
                heading_text.clear();
            }
            Event::Text(t) if in_heading => heading_text.push_str(&t),
            Event::End(TagEnd::Heading(_)) => {
                if let Some((_, slot)) = stack.last_mut() {
                    *slot = heading_text.trim().to_string();
                }
                in_heading = false;
            }
            Event::End(TagEnd::Paragraph) => {
                push_block(&mut blocks, BlockKind::Paragraph, range, &stack)
            }
            Event::End(TagEnd::CodeBlock) => {
                push_block(&mut blocks, BlockKind::Code, range, &stack)
            }
            Event::End(TagEnd::Table) => push_block(&mut blocks, BlockKind::Table, range, &stack),
            Event::End(TagEnd::List(_)) => push_block(&mut blocks, BlockKind::List, range, &stack),
            _ => {}
        }
    }
    blocks.retain(|block| {
        !excluded
            .iter()
            .any(|span| block.range.start < span.end && span.start < block.range.end)
    });
    blocks
}

fn push_block(
    blocks: &mut Vec<Block>,
    kind: BlockKind,
    range: Range<usize>,
    stack: &[(u8, String)],
) {
    if range.start >= range.end {
        return;
    }
    let heading: Vec<String> = stack
        .iter()
        .map(|(_, text)| text.clone())
        .filter(|text| !text.is_empty())
        .collect();
    blocks.push(Block {
        kind,
        range,
        heading,
    });
}

/// Largest suffix of `text` whose token estimate fits `max_tokens`.
/// Walks char indices from the end so the cut always lands on a boundary.
fn overlap_suffix(text: &str, max_tokens: u32) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut best = text.len();
    for (idx, _) in text.char_indices().rev() {
        if estimate_tokens(&text[idx..]) > max_tokens {
            break;
        }
        best = idx;
    }
    text[best..].to_string()
}

/// Manual, serde-free JSON locator string.
fn locator_json(
    start_byte: usize,
    end_byte: usize,
    start_line: u32,
    end_line: u32,
    truncated: bool,
) -> String {
    format!(
        "{{\"start_byte\":{start_byte},\"end_byte\":{end_byte},\"start_line\":{start_line},\"end_line\":{end_line},\"truncated\":{truncated}}}"
    )
}

fn line_starts(text: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (idx, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push((idx + 1) as u32);
        }
    }
    starts
}

/// 1-based line number containing `byte_offset`.
fn line_number_for(starts: &[u32], byte_offset: u32) -> u32 {
    starts.partition_point(|&s| s <= byte_offset) as u32
}

/// Chunk normalized Markdown into policy-bounded chunks of verbatim text.
///
/// Blocks are packed greedily while the joined text stays within
/// `max_chunk_tokens`. Packing resets when the heading stack changes and
/// when a block would overflow. A block whose own estimate exceeds the
/// budget is emitted atomically as its own chunk with `truncated: true` in
/// the locator. Closing a chunk on overflow seeds the next chunk with a
/// token-bounded suffix of the closing chunk's last block when both sides
/// are text-kind (never into or out of Code/Table). The locator's
/// `start_byte` points at the chunk's first real block; an overlap prefix
/// is part of the chunk text but is not counted in the locator bytes.
pub fn chunk_markdown(
    source_key: &str,
    revision_hash: &str,
    normalized: &str,
    policy: &IndexPolicy,
) -> Vec<Chunk> {
    chunk_markdown_with(
        source_key,
        revision_hash,
        normalized,
        policy,
        SourceOffset::default(),
        &[],
        None,
    )
}

/// Where a slice of text starts in the note it was cut from.
///
/// `chunk_markdown` reads byte ranges and line numbers off the text it is handed.
/// When that text is the *body* of a note — the note minus its frontmatter block —
/// those ranges are relative to the slice, while the locator must address the
/// original bytes so citation verification can slice the document directly. Pass
/// the cut here and the locators come back source-relative. (D41 in
/// `docs/docs/ingest-interop.md` puts the same obligation on the wiring side.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceOffset {
    /// Bytes cut off the front of the note.
    pub bytes: usize,
    /// Lines cut off the front of the note.
    pub lines: u32,
}

impl SourceOffset {
    /// The offset a frontmatter block occupies: the note's text, and the body cut
    /// from it.
    pub fn before_body(note: &str, body: &str) -> Self {
        debug_assert!(
            note.ends_with(body),
            "the body must be a suffix of the note it was cut from"
        );
        let bytes = note.len() - body.len();
        let lines = note[..bytes].matches('\n').count() as u32;
        SourceOffset { bytes, lines }
    }
}

/// [`chunk_markdown`] for a slice of a larger note: locators are shifted by `offset`,
/// and the byte spans in `excluded` never become chunk text.
///
/// `excluded` is how a span the caller knows is noise — the Excalidraw scene
/// fence, thousands of bytes of compressed drawing data (§5 第 9 条) — stays out of the
/// index without rewriting a single byte of the note: a block that intersects such
/// a span is dropped whole, so every other chunk keeps the locator it would have
/// had on its own. A chunk built from blocks on both sides of an excluded span has
/// a locator that spans the window between them (an exclusion shifts nothing).
///
/// `rendered_from` names the file a *rendered* text was built from — a `.canvas`,
/// whose text comes from its JSON fields (§5 第 9 条). No byte range inside that file
/// spells the text out, so its chunks carry the file's own axes as a whole and a
/// citation still resolves to the document; `offset` does not apply to them.
pub fn chunk_markdown_with(
    source_key: &str,
    revision_hash: &str,
    normalized: &str,
    policy: &IndexPolicy,
    offset: SourceOffset,
    excluded: &[Range<usize>],
    rendered_from: Option<&str>,
) -> Vec<Chunk> {
    if normalized.trim().is_empty() {
        return Vec::new();
    }
    let blocks = collect_blocks(normalized, excluded);
    if blocks.is_empty() {
        return Vec::new();
    }

    let starts = line_starts(normalized);
    let rendered_axes = rendered_from.map(whole_file_axes);
    let max_tokens = policy.max_chunk_tokens;
    let overlap_tokens = policy.chunk_overlap_tokens;

    /// Chunk being packed. `first_*` describe the first real block; `last_*`
    /// tracks the trailing block for overlap seeding.
    struct Open {
        text: String,
        first_range: Range<usize>,
        first_stack: Vec<String>,
        last_range: Range<usize>,
        last_is_text: bool,
        truncated: bool,
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut ordinal: u32 = 0;

    let mut emit = |open: Open| {
        let token_estimate = estimate_tokens(&open.text);
        let heading_path = if open.first_stack.is_empty() {
            None
        } else {
            Some(open.first_stack.join(" > "))
        };
        let chunk_id = hash::chunk_id(source_key, revision_hash, ordinal);
        let chunk_hash = hash::chunk_hash(&chunk_id, &open.text);
        // The locator addresses the text this chunker was handed — the NORMALISED
        // note (BOM stripped, CRLF to LF), not the file's own bytes; a consumer
        // that wants to slice the file must normalise first (or carry a byte map).
        // Within that text, `open.text` may be longer than `last_range` (packed
        // blocks insert "\n\n" where the source has one "\n"), so end_byte/end_line
        // come from the last block's own span, never from the joined text length.
        // `offset` shifts both axes back into the note when this text is a body
        // slice; a rendered text has no slice to point into, so it carries the
        // axes of the file it was rendered from instead (§5 第 9 条). `truncated`
        // describes the text either way, so it stays the packing loop's flag.
        let (start_byte, end_byte, start_line, end_line) = rendered_axes.unwrap_or_else(|| {
            (
                offset.bytes + open.first_range.start,
                offset.bytes + open.last_range.end,
                offset.lines + line_number_for(&starts, open.first_range.start as u32),
                offset.lines + line_number_for(&starts, open.last_range.end as u32),
            )
        });
        chunks.push(Chunk {
            chunk_id,
            source_key: source_key.to_string(),
            revision_hash: revision_hash.to_string(),
            ordinal,
            heading_path,
            text: open.text,
            locator_json: locator_json(start_byte, end_byte, start_line, end_line, open.truncated),
            chunk_hash,
            token_estimate,
        });
        ordinal += 1;
    };

    let mut current: Option<Open> = None;
    for block in blocks {
        let block_text = &normalized[block.range.clone()];
        let own_tokens = estimate_tokens(block_text);
        let atomic = own_tokens > max_tokens;

        // Decide whether the open chunk must close before this block.
        let mut close = false;
        let mut seed_overlap = false;
        if let Some(open) = &current {
            if atomic || open.first_stack != block.heading {
                close = true;
            } else {
                let joined = estimate_tokens(&format!("{}\n\n{}", open.text, block_text));
                if joined > max_tokens {
                    close = true;
                    seed_overlap = true;
                }
            }
        }

        if close {
            let open = current.take().expect("close implies open chunk");
            let seed = if seed_overlap && open.last_is_text && block.kind.is_text_kind() {
                // The seed is the *front* of a chunk the packing loop then stops
                // checking: `joined > max_tokens` above only decides to close the
                // old chunk. Leave room for the joining separator and for the
                // block that comes with it, or the chunk opens over budget —
                // measured at 685 tokens for a configured 512 with overlap 256.
                let tail = estimate_tokens(&format!("\n\n{block_text}"));
                let budget = overlap_tokens.min(max_tokens.saturating_sub(tail));
                overlap_suffix(&normalized[open.last_range.clone()], budget)
            } else {
                String::new()
            };
            emit(open);
            let text = if seed.is_empty() {
                block_text.to_string()
            } else {
                format!("{}\n\n{}", seed, block_text)
            };
            current = Some(Open {
                text,
                first_range: block.range.clone(),
                first_stack: block.heading.clone(),
                last_range: block.range.clone(),
                last_is_text: block.kind.is_text_kind(),
                truncated: atomic,
            });
            continue;
        }

        if atomic {
            // Lone oversized block: never split, flagged truncated.
            emit(Open {
                text: block_text.to_string(),
                first_range: block.range.clone(),
                first_stack: block.heading.clone(),
                last_range: block.range.clone(),
                last_is_text: block.kind.is_text_kind(),
                truncated: true,
            });
            continue;
        }

        match current.as_mut() {
            Some(open) => {
                open.text.push_str("\n\n");
                open.text.push_str(block_text);
                open.last_range = block.range.clone();
                open.last_is_text = block.kind.is_text_kind();
            }
            None => {
                current = Some(Open {
                    text: block_text.to_string(),
                    first_range: block.range.clone(),
                    first_stack: block.heading.clone(),
                    last_range: block.range.clone(),
                    last_is_text: block.kind.is_text_kind(),
                    truncated: false,
                });
            }
        }
    }
    if let Some(open) = current {
        emit(open);
    }
    chunks
}

/// The axes a rendered text's chunks carry: the file it was rendered from, whole.
///
/// A `.canvas`'s text comes from its JSON fields, so no byte range inside the file
/// spells it out; pointing at the file keeps a citation resolvable — a consumer
/// slices the document and gets the document. Node-level precision inside a canvas
/// is a separate, later job.
fn whole_file_axes(normalized: &str) -> (usize, usize, u32, u32) {
    let starts = line_starts(normalized);
    let end_line = line_number_for(&starts, normalized.len() as u32);
    (0, normalized.len(), 1, end_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use momotaro_contracts::IndexPolicy;

    fn policy_with(max: u32, overlap: u32) -> IndexPolicy {
        IndexPolicy {
            max_chunk_tokens: max,
            chunk_overlap_tokens: overlap,
            ..IndexPolicy::default()
        }
    }

    #[test]
    fn a_body_slice_keeps_locators_on_the_original_note() {
        let note = "---\ntitle: A\n---\n# Heading\n\nbody text here\n";
        let (frontmatter, body) = crate::frontmatter::split(note);
        assert!(matches!(
            frontmatter,
            crate::frontmatter::FrontmatterParse::Parsed(_)
        ));

        let offset = SourceOffset::before_body(note, body);
        let chunks = chunk_markdown_with(
            "note:a.md",
            "rh",
            body,
            &IndexPolicy::default(),
            offset,
            &[],
            None,
        );
        assert_eq!(chunks.len(), 1, "{chunks:?}");

        // The chunk's first block is the paragraph (the heading is structure, and
        // lands in `heading_path`), so the locator's start is the paragraph's
        // offset *in the note* — not its offset in the body slice.
        let paragraph_at = note
            .find("body text here")
            .expect("the paragraph is in the note");
        assert!(
            chunks[0]
                .locator_json
                .contains(&format!("\"start_byte\":{paragraph_at}")),
            "the locator addresses the note, not the slice: {}",
            chunks[0].locator_json
        );
        assert!(
            chunks[0].locator_json.contains("\"start_line\":6"),
            "line numbers are relative to the note too: {}",
            chunks[0].locator_json
        );
        assert!(
            note[paragraph_at..].starts_with("body text here"),
            "and slicing the note at the locator lands on the paragraph"
        );
        assert_eq!(
            chunks[0].heading_path.as_deref(),
            Some("Heading"),
            "the block is out of the body but its heading still frames the chunk"
        );
    }

    #[test]
    fn zh_headings_carry_heading_path() {
        let doc = "# 标题\n\n引言段落。\n\n## 章节\n\n正文第一段。\n\n正文第二段。\n";
        let chunks = chunk_markdown("zh.md", "rh", doc, &IndexPolicy::default());
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].heading_path.as_deref(), Some("标题"));
        assert_eq!(chunks[0].text, "引言段落。\n");
        assert_eq!(chunks[1].heading_path.as_deref(), Some("标题 > 章节"));
        // Block slices are verbatim (each paragraph carries its trailing
        // newline); packing joins them with the literal "\n\n" separator.
        assert_eq!(chunks[1].text, "正文第一段。\n\n\n正文第二段。\n");
    }

    #[test]
    fn oversized_code_block_is_atomic_and_truncated() {
        let code = "x".repeat(200);
        let doc = format!("```rust\n{code}\n```\n");
        let chunks = chunk_markdown("code.md", "rh", &doc, &policy_with(50, 8));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains(&code));
        assert!(chunks[0].text.starts_with("```rust"));
        assert!(chunks[0].locator_json.contains("\"truncated\":true"));
        assert!(chunks[0].token_estimate > 50);
    }

    #[test]
    fn oversized_table_is_atomic_and_truncated() {
        let mut doc = String::from("| col | value |\n|---|---|\n");
        for i in 0..12 {
            let filler = "x".repeat(20);
            doc.push_str(&format!("| r{i} | {filler} |\n"));
        }
        let chunks = chunk_markdown("table.md", "rh", &doc, &policy_with(50, 8));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.starts_with("| col |"));
        assert!(chunks[0].locator_json.contains("\"truncated\":true"));
    }

    #[test]
    fn long_doc_packs_within_budget_with_overlap() {
        let mut doc = String::from("# 综述\n\n");
        for i in 0..40 {
            doc.push_str(&format!(
                "第{i}段：矩阵分解为特征值与特征向量，eigen decomposition of the matrix，谱定理成立。\n\n"
            ));
        }
        let chunks = chunk_markdown("long.md", "rh", &doc, &IndexPolicy::default());
        assert!(chunks.len() >= 3, "expected multiple chunks");
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.ordinal as usize, i);
            assert!(
                chunk.token_estimate <= 512,
                "chunk {i} exceeds budget: {}",
                chunk.token_estimate
            );
        }
        for i in 0..chunks.len() - 1 {
            let prev = &chunks[i].text;
            let last_block_start = prev.rfind("\n\n").map(|p| p + 2).unwrap_or(0);
            let last_block = &prev[last_block_start..];
            let seed = overlap_suffix(last_block, 64);
            assert!(
                !seed.is_empty() && chunks[i + 1].text.starts_with(&seed),
                "chunk {} must start with a suffix of chunk {}",
                i + 1,
                i
            );
        }
    }

    #[test]
    fn empty_and_heading_only_docs_yield_no_chunks() {
        assert!(chunk_markdown("e.md", "rh", "", &IndexPolicy::default()).is_empty());
        assert!(chunk_markdown("w.md", "rh", "  \n\n\t\n", &IndexPolicy::default()).is_empty());
        assert!(
            chunk_markdown("h.md", "rh", "# Only a heading\n", &IndexPolicy::default()).is_empty()
        );
    }

    #[test]
    fn single_paragraph_chunk_is_verbatim() {
        let doc = "Hello world, this is verbatim.";
        let chunks = chunk_markdown("v.md", "rh", doc, &IndexPolicy::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, doc);
        assert!(chunks[0].heading_path.is_none());
        assert!(chunks[0].locator_json.contains("\"start_byte\":0"));
        assert!(chunks[0].locator_json.contains("\"end_byte\":30"));
        assert!(chunks[0].locator_json.contains("\"start_line\":1"));
        assert!(chunks[0].locator_json.contains("\"end_line\":1"));
        assert!(chunks[0].locator_json.contains("\"truncated\":false"));
    }

    #[test]
    fn identical_content_different_keys_differ_in_chunk_id() {
        let doc = "same text\n\nmore text\n";
        let a = chunk_markdown("a.md", "rh", doc, &IndexPolicy::default());
        let b = chunk_markdown("b.md", "rh", doc, &IndexPolicy::default());
        assert_eq!(a.len(), b.len());
        assert_ne!(a[0].chunk_id, b[0].chunk_id);
    }

    #[test]
    fn code_block_never_shares_a_chunk_with_neighbors() {
        let doc = "para one\n\n```txt\ncode\n```\n\npara two\n";
        let chunks = chunk_markdown("m.md", "rh", doc, &policy_with(500, 64));
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.starts_with("para one"));
        assert!(chunks[0].text.contains("```txt\ncode\n```"));
        assert!(chunks[0].text.trim_end().ends_with("para two"));
    }

    /// The locator is the citation contract: its byte span must stay inside
    /// the source document and point at the chunk's own content, even when
    /// packing rewrote internal separators ("\n" -> "\n\n").
    #[test]
    fn locator_bytes_stay_within_source_document() {
        let mut doc = String::from("# 越界回归\n\n");
        for i in 0..30 {
            doc.push_str(&format!(
                "第{i}段：矩阵分解为特征值与特征向量，spectral theorem holds for symmetric matrices。\n\n"
            ));
        }
        let chunks = chunk_markdown("bounds.md", "rh", &doc, &IndexPolicy::default());
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            let locator: serde_json::Value =
                serde_json::from_str(&chunk.locator_json).expect("locator is valid JSON");
            let start_byte = locator["start_byte"].as_u64().expect("start_byte") as usize;
            let end_byte = locator["end_byte"].as_u64().expect("end_byte") as usize;
            assert!(
                end_byte <= doc.len(),
                "end_byte {end_byte} exceeds doc len {}",
                doc.len()
            );
            assert!(start_byte < end_byte);
            // The span must address the chunk's real content. The doc slice
            // begins at the chunk's first real block; with an overlap seed
            // the chunk text starts earlier, but the block text itself must
            // appear inside the chunk body.
            let doc_slice = &doc[start_byte..end_byte];
            let probe: String = doc_slice.chars().take(20).collect();
            assert!(
                chunk.text.contains(&probe),
                "locator span does not address the chunk body (probe {probe:?})"
            );
        }
    }

    /// An excluded span is dropped whole and nothing around it moves: the note's
    /// bytes are never rewritten, so a chunk's locator still addresses the note
    /// (§5 第 9 条 — the Excalidraw scene fence).
    #[test]
    fn an_excluded_span_leaves_the_locators_around_it_alone() {
        let doc = "beforetoken\n\n```compressed-json\nDRAWINGNOISE\n```\n\naftertoken\n";
        let opener = doc.find("```compressed-json").expect("the fence");
        let closer = doc.find("\n```\n").expect("the closing fence");
        let span = opener..closer + 4;
        assert_eq!(&doc[span.clone()], "```compressed-json\nDRAWINGNOISE\n```");

        let chunks = chunk_markdown_with(
            "scene.md",
            "rh",
            doc,
            &IndexPolicy::default(),
            SourceOffset::default(),
            &[span],
            None,
        );
        let text: String = chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("beforetoken"), "{text:?}");
        assert!(text.contains("aftertoken"), "{text:?}");
        assert!(!text.contains("DRAWINGNOISE"), "the span is out: {text:?}");

        // The window still ends where the note's own last block ends: an
        // implementation that deleted the bytes would end it earlier.
        let survivor = chunks
            .iter()
            .find(|chunk| chunk.text.contains("aftertoken"))
            .expect("a chunk with the text after the fence");
        let locator: serde_json::Value =
            serde_json::from_str(&survivor.locator_json).expect("locator is valid JSON");
        let after_at = doc.find("aftertoken").expect("the text after the fence");
        assert_eq!(
            locator["end_byte"].as_u64(),
            // The paragraph's span runs through the newline that ends it — the
            // parser's own end offset — so the window closes just past that byte.
            Some((after_at + "aftertoken\n".len()) as u64),
            "the locator addresses the note: {}",
            survivor.locator_json
        );
    }

    #[test]
    fn an_excluded_span_that_covers_everything_yields_no_chunks() {
        let doc = "```compressed-json\nDRAWINGNOISE\n```\n";
        let span = 0..doc.trim_end_matches('\n').len();
        let chunks = chunk_markdown_with(
            "only.md",
            "rh",
            doc,
            &IndexPolicy::default(),
            SourceOffset::default(),
            &[span],
            None,
        );
        assert!(chunks.is_empty(), "{chunks:?}");
    }

    /// A rendered text's chunks point at the file it was rendered from, whatever
    /// the text itself says: the byte span is the whole document, so a citation
    /// resolves to the document (§5 第 9 条).
    #[test]
    fn a_rendered_text_points_at_the_file_it_came_from() {
        let doc = "# Title\n\nbody\n";
        let rendered = "card one\n\ncard two\n";
        let chunks = chunk_markdown_with(
            "board.canvas",
            "rh",
            rendered,
            &IndexPolicy::default(),
            SourceOffset::default(),
            &[],
            Some(doc),
        );
        assert!(!chunks.is_empty(), "the rendered text is chunked");
        for chunk in &chunks {
            let value: serde_json::Value =
                serde_json::from_str(&chunk.locator_json).expect("locator is valid JSON");
            assert_eq!(value["start_byte"].as_u64(), Some(0), "{value}");
            assert_eq!(
                value["end_byte"].as_u64(),
                Some(doc.len() as u64),
                "{value}"
            );
            assert_eq!(value["start_line"].as_u64(), Some(1), "{value}");
            // The file ends in a newline, so its last line is empty — the counting
            // the chunker does for every other chunk too.
            assert_eq!(value["end_line"].as_u64(), Some(4), "{value}");
        }
    }

    /// Only the coordinate axes come from the file: `truncated` describes the
    /// rendered text (a block too big for the budget), so it must survive.
    #[test]
    fn a_rendered_chunk_keeps_its_truncation_flag() {
        let rendered = format!("{}\n", "很长的卡片文本".repeat(200));
        let chunks = chunk_markdown_with(
            "big.canvas",
            "rh",
            &rendered,
            &policy_with(10, 0),
            SourceOffset::default(),
            &[],
            Some("{}"),
        );
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert!(
            chunks[0].locator_json.contains("\"truncated\":true"),
            "{}",
            chunks[0].locator_json
        );
    }
}
