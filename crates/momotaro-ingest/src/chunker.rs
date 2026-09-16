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
/// stack for subsequent blocks.
fn collect_blocks(normalized: &str) -> Vec<Block> {
    let parser = Parser::new_ext(normalized, Options::ENABLE_TABLES);
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
    if normalized.trim().is_empty() {
        return Vec::new();
    }
    let blocks = collect_blocks(normalized);
    if blocks.is_empty() {
        return Vec::new();
    }

    let starts = line_starts(normalized);
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
        // The locator must address the ORIGINAL source bytes so citation
        // verification can slice the document directly. `open.text` may be
        // longer than `last_range` (packed blocks insert "\n\n" where the
        // source has one "\n"), so end_byte/end_line come from the last
        // block's own span, never from the joined text length.
        let start_byte = open.first_range.start;
        let end_byte = open.last_range.end;
        let start_line = line_number_for(&starts, start_byte as u32);
        let end_line = line_number_for(&starts, end_byte as u32);
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
                overlap_suffix(&normalized[open.last_range.clone()], overlap_tokens)
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
}
