//! Link extraction and resolution for graph edge building (`§5 第 2–4 条`).
//!
//! Everything here is pure: text in, records and answers out. The store and the
//! vault never enter; what the caller does with a [`RawLink`] (store it in
//! `metadata_json.links`, project it into `graph_links`) is `run`'s wiring. The
//! parse base is the same one the chunker uses, so a wikilink is a link in
//! exactly one place's opinion (`§5 第 6 条`).

use std::collections::HashMap;
use std::ops::Range;

use momotaro_contracts::{AnchorKind, Degradation, EdgeKind, LinkSource, nfc_text};
use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd};

/// One extracted link record — the `metadata_json.links` shape (`§5 第 2 条`).
///
/// `raw_target` and `anchor_raw` are exactly as written: a markdown link's
/// target keeps its percent-encoding, and re-projection decodes it again
/// (rules upgrade ⇒ replay without the vault). Two writings of the same target
/// are two records; first occurrences win the identity dedupe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLink {
    /// The target as written (the part before `#`), never decoded here.
    pub raw_target: String,
    /// How the edge is expressed.
    pub edge_kind: EdgeKind,
    /// Which syntax produced it.
    pub link_source: LinkSource,
    /// The anchor's kind; `None` when the link has no anchor.
    pub anchor_kind: Option<AnchorKind>,
    /// The anchor as written (the part after `#`, including a `^`).
    pub anchor_raw: Option<String>,
}

/// What one extraction pass found (`§5 第 2 条` / `§5 第 7 条`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    /// Buildable records, document order, identity-deduped (first wins).
    pub links: Vec<RawLink>,
    /// Distinct extraction losses, declaration order — the `link_losses` blob.
    pub losses: Vec<Degradation>,
}

impl Extracted {
    /// Records one distinct extraction loss (§5 第 7 条: the list is codes,
    /// once each, declaration order by construction).
    pub fn push_loss(&mut self, code: Degradation) {
        if !self.losses.contains(&code) {
            self.losses.push(code);
            self.losses.sort_unstable();
        }
    }

    /// Records one link: the same writing twice is one row (ADR 0027), first
    /// occurrence keeps its place in the document order.
    pub fn push_link(&mut self, link: RawLink) {
        if !self.links.contains(&link) {
            self.links.push(link);
        }
    }

    /// Absorbs another pass's findings (body + frontmatter extraction).
    pub fn absorb(&mut self, other: Extracted) {
        for link in other.links {
            self.push_link(link);
        }
        for code in other.losses {
            self.push_loss(code);
        }
    }
}

/// One parse of a Markdown body: the links it writes and the outline its
/// anchors are checked against (`§5 第 2/4 条`). One event walk feeds both —
/// the parse base has one opinion of the text, and the text is parsed once.
///
/// `excluded` byte spans (the Excalidraw scene) are skipped like the chunker
/// skips them: content inside one is not the note's prose. Code spans and code
/// fences keep their `[[…]]` text — that is someone's code, not a link, and
/// not a remnant (`§5 第 6 条`).
pub fn analyze(text: &str, excluded: &[Range<usize>]) -> (Extracted, Outline) {
    let mut out = Extracted::default();
    let mut outline = Outline::default();
    let parser = Parser::new_ext(text, crate::PARSE_OPTIONS);

    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut heading_text = String::new();
    let mut in_heading = false;
    let mut code_spans: Vec<Range<usize>> = Vec::new();
    let mut in_code_block = false;
    // The parser splits a refused `[[…]]` into per-character Text pieces, so
    // remnants are only visible across adjacent Text events: accumulate a run
    // and scan it at every **block** boundary. Inline marks (emphasis and
    // friends) do not end a run — a remnant may carry formatting inside.
    let mut text_run = String::new();

    for (event, range) in parser.into_offset_iter() {
        if excluded
            .iter()
            .any(|span| range.start < span.end && span.start < range.end)
        {
            // The excluded span breaks the text run: pairing a `[[` from before
            // it with a `]]` from after would invent a remnant nobody wrote.
            scan_remnants(&text_run, &mut out);
            text_run.clear();
            continue;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                scan_remnants(&text_run, &mut out);
                text_run.clear();
                let lvl = level as u8;
                while stack.last().is_some_and(|(l, _)| *l >= lvl) {
                    stack.pop();
                }
                stack.push((lvl, String::new()));
                in_heading = true;
                heading_text.clear();
            }
            Event::Text(t) if in_heading => heading_text.push_str(&t),
            // Inline code inside a heading is heading text (the outline and
            // the `heading_path` both stand on this collector), and the
            // parser emits `Event::Code` beside `Event::Text`. The body's
            // `text_run` deliberately does not collect code spans: a `[[x]]`
            // written inside inline code is not a link.
            Event::Code(t) if in_heading => heading_text.push_str(&t),
            Event::Text(t) if !in_code_block => text_run.push_str(&t),
            Event::End(TagEnd::Heading(_)) => {
                // A heading collects its text into a buffer of its own, so
                // the body's trailing scan never sees it: a `[[|alias]]` with
                // no destination that pulldown-cmark did not turn into a link
                // would vanish silently from a `#` heading while the same text
                // in a paragraph is reported (`§5 第 6 条` names this shape).
                scan_remnants(&heading_text, &mut out);
                if let Some((_, slot)) = stack.last_mut() {
                    *slot = heading_text.trim().to_string();
                }
                in_heading = false;
                let path: Vec<String> = stack
                    .iter()
                    .map(|(_, text)| text.clone())
                    .filter(|text| !text.is_empty())
                    .collect();
                outline.headings.push(path);
            }
            Event::Start(Tag::CodeBlock(_)) => {
                scan_remnants(&text_run, &mut out);
                text_run.clear();
                code_spans.push(range);
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => in_code_block = false,
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                scan_remnants(&text_run, &mut out);
                text_run.clear();
                classify_event_link(&dest_url, link_type, false, &mut out);
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => {
                scan_remnants(&text_run, &mut out);
                text_run.clear();
                classify_event_link(&dest_url, link_type, true, &mut out);
            }
            // Block ends and raw HTML close the run; inline marks (Em, Strong,
            // …) deliberately do not.
            Event::End(TagEnd::Paragraph)
            | Event::End(TagEnd::Table)
            | Event::End(TagEnd::List(_))
            | Event::End(TagEnd::Item)
            | Event::Html(_)
            | Event::InlineHtml(_) => {
                scan_remnants(&text_run, &mut out);
                text_run.clear();
            }
            _ => {}
        }
    }
    scan_remnants(&text_run, &mut out);

    // A `^id` definition is a line-final caret id outside any fenced code
    // block (§5 第 4 条: end of a paragraph, its own line, or after a bullet) —
    // inside a fence it is code someone wrote, not a block definition.
    let mut offset = 0;
    for line in text.lines() {
        let end = offset + line.len();
        let in_code = code_spans
            .iter()
            .any(|span| offset < span.end && span.start < end);
        if !in_code && let Some(id) = line_final_block_id(line.trim_end()) {
            outline.block_ids.push(id.to_owned());
        }
        offset = end + 1;
    }
    (out, outline)
}

/// The link half of [`analyze`] — split out for focused tests.
#[cfg(test)]
fn extract_links(text: &str, excluded: &[Range<usize>]) -> Extracted {
    analyze(text, excluded).0
}

/// The outline half of [`analyze`] — split out for focused tests.
#[cfg(test)]
fn outline_of(text: &str) -> Outline {
    analyze(text, &[]).1
}

/// Extracts the links a frontmatter block carries (`§5 第 5 条`).
///
/// Always from the `raw` block text, so an unmodelled field shape cannot hide a
/// link: only column-0 `#` comment lines are skipped, and block-scalar content
/// is content — its links build edges. Parsed and failed blocks alike call this.
pub fn extract_frontmatter_links(raw: &str) -> Extracted {
    let mut out = Extracted::default();
    for line in raw.lines() {
        if line.starts_with('#') {
            continue;
        }
        scan_frontmatter_line(line, &mut out);
    }
    out
}

/// The target-side facts an anchor is checked against (`§5 第 4 条`).
///
/// `headings` holds one path per heading — the h1-h6 stack snapshot after that
/// heading, empty levels filtered — mirroring the chunker's `heading_path`
/// semantics exactly. `block_ids` holds every line-final `^id` definition.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outline {
    /// One entry per heading, level texts in stack order.
    pub headings: Vec<Vec<String>>,
    /// `^id` definitions seen at end of line, without the caret.
    pub block_ids: Vec<String>,
}

/// A line-final `^id` is a block definition in all three shapes (`§5 第 4 条`):
/// end of a paragraph line, on its own line, or after a bullet. The caret must
/// start the line or follow whitespace, and the id must run to end of line.
fn line_final_block_id(trimmed: &str) -> Option<&str> {
    let idx = trimmed.rfind('^')?;
    let before_ok = idx == 0 || trimmed[..idx].ends_with(char::is_whitespace);
    let id = &trimmed[idx + 1..];
    if before_ok && is_block_id(id) {
        Some(id)
    } else {
        None
    }
}

/// Whether an anchor still exists in its target's outline (`§5 第 4 条`).
///
/// Heading anchors are text anchors: the levels of `#H1#H2` match the tail of
/// some heading's path (a single `#H` therefore matches any heading named `H`,
/// first occurrence counting for locating, not for existence). Block anchors
/// match a `^id` definition. Both sides are compared NFC-folded but never
/// case-folded (D42).
pub fn anchor_still_exists(kind: AnchorKind, anchor_raw: &str, outline: &Outline) -> bool {
    match kind {
        AnchorKind::Block => {
            let id = anchor_raw.strip_prefix('^').unwrap_or(anchor_raw);
            outline.block_ids.iter().any(|seen| seen == id)
        }
        AnchorKind::Heading => {
            let levels: Vec<String> = anchor_raw
                .split('#')
                .map(|level| nfc_text(level.trim()))
                .collect();
            if levels.is_empty() || levels.iter().any(|level| level.is_empty()) {
                return false;
            }
            outline.headings.iter().any(|path| {
                path.len() >= levels.len()
                    && path[path.len() - levels.len()..]
                        .iter()
                        .zip(&levels)
                        .all(|(seen, want)| nfc_text(seen) == *want)
            })
        }
    }
}

/// A vault's linkable names: what a raw target can resolve against (`§5 第 3 条`).
#[derive(Debug, Default)]
pub struct Namespace {
    /// Vault-relative path (`docs/a.md`) → source key.
    by_path: HashMap<String, String>,
    /// Basename or stem → the sources carrying it (for bare-name matching).
    by_name: HashMap<String, Vec<(String, String)>>,
}

impl Namespace {
    /// Builds the namespace from `(vault_relative_path, source_key)` pairs —
    /// typically one walk of the vault.
    pub fn new<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut ns = Namespace::default();
        for (path, key) in entries {
            let path = nfc_text(path);
            ns.by_path.insert(path.clone(), key.to_owned());
            if let Some(name) = path.rsplit('/').next() {
                ns.by_name
                    .entry(name.to_owned())
                    .or_default()
                    .push((key.to_owned(), path.clone()));
                // The stem key is name minus its LAST extension, so `a.b.md`
                // stems to `a.b` and `img.png` to `img`.
                if let Some((stem, _)) = name.rsplit_once('.')
                    && !stem.is_empty()
                {
                    ns.by_name
                        .entry(stem.to_owned())
                        .or_default()
                        .push((key.to_owned(), path.clone()));
                }
            }
        }
        ns
    }
}

/// What a raw target resolved to (`§5 第 2–3 条`).
#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    /// One target key.
    Resolved(String),
    /// No candidate: the row is dangling, `raw_target` stays.
    Dangling,
    /// Tied candidates: no edge, an `ambiguous_wikilink` lists the keys.
    Ambiguous(Vec<String>),
}

/// Resolves one record's raw target against a [`Namespace`] (`§5 第 3 条`).
///
/// Markdown targets are percent-decoded first (a record is only ever born from
/// a successfully decoded link, so the failure branch is defensive); `./` is
/// stripped; a `..` component never becomes a key (same rule as the key layer).
/// Path forms match exactly (or with `.md` appended); bare names match a
/// basename or stem. Ties prefer the link's own directory; still tied means
/// no edge — never a guess.
pub fn resolve(
    raw_target: &str,
    from_dir: &str,
    link_source: LinkSource,
    ns: &Namespace,
) -> Resolution {
    let decoded;
    let target = if link_source == LinkSource::Markdown {
        match percent_decode(raw_target) {
            Some(text) => {
                decoded = text;
                decoded.as_str()
            }
            // Defensive: extraction never stores an undecodable target.
            None => return Resolution::Dangling,
        }
    } else {
        raw_target
    };
    let stripped = target.trim_start_matches("./");
    let target = nfc_text(stripped);

    if target.is_empty() {
        return Resolution::Dangling;
    }
    if target.split('/').any(|component| component == "..") {
        return Resolution::Dangling;
    }

    let mut candidates: Vec<(String, String)> = Vec::new();
    if target.contains('/') {
        for suffix in ["", ".md"] {
            if let Some(key) = ns.by_path.get(&format!("{target}{suffix}")) {
                candidates.push((key.clone(), format!("{target}{suffix}")));
            }
        }
    } else {
        if let Some(hits) = ns.by_name.get(&target) {
            candidates.extend(hits.iter().cloned());
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);

    match candidates.len() {
        0 => Resolution::Dangling,
        1 => Resolution::Resolved(candidates.remove(0).0),
        _ => {
            let same_dir: Vec<(String, String)> = candidates
                .iter()
                .filter(|(_, path)| parent_dir(path) == from_dir)
                .cloned()
                .collect();
            if same_dir.len() == 1 {
                Resolution::Resolved(same_dir[0].0.clone())
            } else {
                let keys = candidates.into_iter().map(|(key, _)| key).collect();
                Resolution::Ambiguous(keys)
            }
        }
    }
}

/// The directory of a vault-relative path (`""` at the vault root).
pub fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// Percent-decodes one target part; `None` on a malformed escape or a result
/// that is not UTF-8 (`§5 第 7 条`: `markdown_link_decode_error`).
pub fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            let value = (hex_val(hi)? << 4) | hex_val(lo)?;
            out.push(value);
            idx += 3;
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_block_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_external(target: &str) -> bool {
    // A URI, not a vault path: `scheme://…` in any spelling, plus `mailto:`.
    // A bare `arxiv:1234`-shaped name has no `//` and stays a (dangling) name.
    // Allocation-free (probe P1-F2): this runs once per link on the indexing
    // hot path, and the case-folding happens per compared byte rather than by
    // lowercasing the whole target first.
    if target
        .get(..7)
        .is_some_and(|head| head.eq_ignore_ascii_case("mailto:"))
    {
        return true;
    }
    target.split_once(':').is_some_and(|(scheme, rest)| {
        rest.starts_with("//")
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// Classifies one link event's destination into a record, a loss, or silence.
fn classify_event_link(dest: &str, link_type: LinkType, embed: bool, out: &mut Extracted) {
    let wikilink = matches!(link_type, LinkType::WikiLink { .. });
    if matches!(link_type, LinkType::Autolink | LinkType::Email) {
        return;
    }
    let (edge_kind, link_source) = match (embed, wikilink) {
        (false, true) => (EdgeKind::Link, LinkSource::Wikilink),
        (false, false) => (EdgeKind::Link, LinkSource::Markdown),
        (true, true) => (EdgeKind::Embed, LinkSource::Embed),
        (true, false) => (EdgeKind::Embed, LinkSource::Markdown),
    };
    classify(dest, edge_kind, link_source, out);
}

/// One classifier for every syntax: split at the first `#`, gate externals,
/// decode markdown parts, then record (or lose) the link (`§5 第 2–6 条`).
fn classify(dest: &str, edge_kind: EdgeKind, link_source: LinkSource, out: &mut Extracted) {
    let (raw_target, raw_anchor) = split_anchor(dest);
    if is_external(raw_target) {
        return;
    }

    let decoded_target;
    let decoded_anchor;
    let anchor = if link_source == LinkSource::Markdown {
        decoded_target = match percent_decode(raw_target) {
            Some(text) => text,
            None => {
                out.push_loss(Degradation::MarkdownLinkDecodeError);
                return;
            }
        };
        if is_external(&decoded_target) {
            return;
        }
        decoded_anchor = match raw_anchor {
            Some(raw) => match percent_decode(raw) {
                Some(text) => Some(text),
                None => {
                    out.push_loss(Degradation::MarkdownLinkDecodeError);
                    return;
                }
            },
            None => None,
        };
        decoded_anchor.as_deref()
    } else {
        raw_anchor
    };

    // Anchor form check (`§5 第 6 条`): `^` anchors need an id in `[A-Za-z0-9-]+`.
    let mut anchor_kind = None;
    let mut anchor_raw_out = None;
    if let Some(anchor_text) = anchor {
        if anchor_text.is_empty() {
            // `[[X#]]` — a degenerate fragment is just no anchor.
        } else if let Some(id) = anchor_text.strip_prefix('^') {
            if is_block_id(id) {
                anchor_kind = Some(AnchorKind::Block);
                anchor_raw_out = Some(raw_anchor.unwrap_or(anchor_text).to_owned());
            } else {
                out.push_loss(Degradation::UnknownLinkSyntax);
                return;
            }
        } else {
            anchor_kind = Some(AnchorKind::Heading);
            anchor_raw_out = Some(raw_anchor.unwrap_or(anchor_text).to_owned());
        }
    }

    if raw_target.is_empty() {
        // `[[#H]]` names this very note: an explicit non-edge, not a lost edge
        // (`§5 第 2 条`). `[[#]]` is nothing at all — a remnant shape.
        if anchor_kind.is_some() {
            return;
        }
        out.push_loss(Degradation::UnknownLinkSyntax);
        return;
    }

    out.push_link(RawLink {
        raw_target: raw_target.to_owned(),
        edge_kind,
        link_source,
        anchor_kind,
        anchor_raw: anchor_raw_out,
    });
}

fn split_anchor(dest: &str) -> (&str, Option<&str>) {
    match dest.split_once('#') {
        Some((target, anchor)) => (target, Some(anchor)),
        None => (dest, None),
    }
}

/// A closed `[[…]]` shape left in text is something the parser refused —
/// `unknown_link_syntax`, never silent (`§5 第 6 条`).
fn scan_remnants(text: &str, out: &mut Extracted) {
    let mut idx = 0;
    while let Some(start) = text[idx..].find("[[") {
        let open = idx + start;
        match text[open + 2..].find("]]") {
            Some(close) => {
                out.push_loss(Degradation::UnknownLinkSyntax);
                idx = open + 2 + close + 2;
            }
            None => break,
        }
    }
}

/// One frontmatter line: same classifier, `frontmatter` source (`§5 第 5 条`).
fn scan_frontmatter_line(line: &str, out: &mut Extracted) {
    let mut rest = line;
    while let Some(open) = rest.find("[[") {
        let embed = rest[..open].ends_with('!');
        match rest[open + 2..].find("]]") {
            Some(close) => {
                let body = &rest[open + 2..open + 2 + close];
                let (dest, _alias) = match body.split_once('|') {
                    Some((dest, alias)) => (dest, Some(alias)),
                    None => (body, None),
                };
                if dest.trim().is_empty() {
                    out.push_loss(Degradation::UnknownLinkSyntax);
                } else {
                    let edge_kind = if embed {
                        EdgeKind::Embed
                    } else {
                        EdgeKind::Link
                    };
                    classify(dest, edge_kind, LinkSource::Frontmatter, out);
                }
                rest = &rest[open + 2 + close + 2..];
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        target: &str,
        edge: EdgeKind,
        source: LinkSource,
        anchor: Option<(AnchorKind, &str)>,
    ) -> RawLink {
        RawLink {
            raw_target: target.to_owned(),
            edge_kind: edge,
            link_source: source,
            anchor_kind: anchor.map(|(kind, _)| kind),
            anchor_raw: anchor.map(|(_, raw)| raw.to_owned()),
        }
    }

    #[test]
    fn a_wikilink_is_recorded_exactly_as_written() {
        let out = extract_links("see [[folder/note#H1#H2|alias]] here", &[]);
        assert_eq!(
            out.links,
            [record(
                "folder/note",
                EdgeKind::Link,
                LinkSource::Wikilink,
                Some((AnchorKind::Heading, "H1#H2"))
            )]
        );
        assert!(out.losses.is_empty());
    }

    #[test]
    fn a_self_loop_is_silence_not_a_lost_edge() {
        let out = extract_links("jump to [[#my heading]] for more", &[]);
        assert!(out.links.is_empty(), "a self-loop builds no edge");
        assert!(out.losses.is_empty(), "and is not a loss either");
    }

    #[test]
    fn an_empty_anchor_is_a_remnant() {
        // `[[#]]` is nothing at all: target empty, anchor empty.
        let out = extract_links("x [[#]] y", &[]);
        assert!(out.links.is_empty());
        assert_eq!(out.losses, [Degradation::UnknownLinkSyntax]);
    }

    #[test]
    fn an_invalid_block_form_is_a_remnant() {
        // `[[#^]]` parses as a link, but its block id is empty — the form check
        // owns this witness alone, so removing its producer goes red here.
        let out = extract_links("y [[#^]] z", &[]);
        assert!(out.links.is_empty());
        assert_eq!(out.losses, [Degradation::UnknownLinkSyntax]);
    }

    #[test]
    fn two_anchors_on_one_target_are_two_records() {
        // The anchor is half of the row identity (ADR 0027): same target, same
        // syntax, different anchors means distinct edges — and collapsing them
        // would be exactly the silent merge the identity exists to refuse.
        let out = extract_links("[[x#A]] [[x#B]] [[x]]", &[]);
        let mut identities: Vec<(String, Option<String>)> = out
            .links
            .iter()
            .map(|link| (link.raw_target.clone(), link.anchor_raw.clone()))
            .collect();
        identities.sort();
        assert_eq!(
            identities,
            [
                ("x".to_owned(), None),
                ("x".to_owned(), Some("A".to_owned())),
                ("x".to_owned(), Some("B".to_owned())),
            ]
        );
    }

    #[test]
    fn an_embed_is_singled_out_on_the_edge_kind_axis() {
        let out = extract_links("![[note#^37066d]]", &[]);
        assert_eq!(
            out.links,
            [record(
                "note",
                EdgeKind::Embed,
                LinkSource::Embed,
                Some((AnchorKind::Block, "^37066d"))
            )]
        );
    }

    /// The markdown half of the embed axis: `![alt](target.md)` is an embed
    /// edge too, and every Embed assertion until this one was written in
    /// wikilink spelling — the `(true, false)` arm of the mapping could fall
    /// to `Link` with the whole suite green.
    #[test]
    fn a_markdown_image_is_an_embed_edge() {
        let out = extract_links("![alt](target.md)", &[]);
        assert_eq!(
            out.links,
            [record(
                "target.md",
                EdgeKind::Embed,
                LinkSource::Markdown,
                None
            )]
        );
    }

    #[test]
    fn a_markdown_link_keeps_its_raw_target_and_decodes_later() {
        let out = extract_links("[t](my%20note.md#Head)", &[]);
        assert_eq!(
            out.links,
            [record(
                "my%20note.md",
                EdgeKind::Link,
                LinkSource::Markdown,
                Some((AnchorKind::Heading, "Head"))
            )],
            "the record holds the writing; decoding is resolution's job"
        );
    }

    #[test]
    fn a_malformed_escape_is_a_decode_loss_not_a_row() {
        let out = extract_links("[t](bad%ZZname.md) and [u](trail%.md)", &[]);
        assert!(out.links.is_empty());
        assert_eq!(out.losses, [Degradation::MarkdownLinkDecodeError]);
    }

    #[test]
    fn external_targets_are_not_vault_edges() {
        let out = extract_links(
            "[a](https://example.com) [b](obsidian://open?vault=x) [c](mailto:x@y)",
            &[],
        );
        assert!(out.links.is_empty(), "externals are not vault edges");
        assert!(out.losses.is_empty(), "and not losses either");
    }

    #[test]
    fn a_remnant_stitches_across_inline_formatting() {
        // The refused `[[|x]]` shape arrives split around the emphasis; the
        // text run must outlive an inline mark to see the closed bracket pair.
        let out = extract_links("a [[|*x*]] b", &[]);
        assert!(out.links.is_empty());
        assert_eq!(out.losses, [Degradation::UnknownLinkSyntax]);
    }

    #[test]
    fn any_uri_scheme_stays_out_of_the_vault() {
        let out = extract_links("[t](file://x) [u](ftp://y) [v](mailto:z@w)", &[]);
        assert!(
            out.links.is_empty(),
            "URIs are not vault edges: {:?}",
            out.links
        );
        assert!(out.losses.is_empty(), "and not losses either");
        let out = extract_links("[w](arxiv:2401.1234)", &[]);
        assert_eq!(
            out.links.len(),
            1,
            "a bare colon name is still a (dangling) name, not a URI"
        );
    }

    #[test]
    fn code_keeps_its_bracket_text_quiet() {
        let out = extract_links("run `arr[[i]]` and:\n\n```\n[[x]]\n```\n", &[]);
        assert!(out.links.is_empty(), "code is not a link");
        assert!(out.losses.is_empty(), "and not a remnant");
    }

    #[test]
    fn a_refused_shape_in_prose_is_reported_once() {
        let out = extract_links("a [[|alias]] b [[|other]] c", &[]);
        assert!(out.links.is_empty());
        assert_eq!(
            out.losses,
            [Degradation::UnknownLinkSyntax],
            "distinct codes, not one per occurrence"
        );
    }

    /// The heading half of `a_refused_shape_in_prose_is_reported_once`.
    ///
    /// `§5 第 6 条` names `[[|alias]]` by shape, and the paragraph case was
    /// covered — but heading text is collected into a separate buffer that
    /// the body's trailing scan never sees, so a refused shape on a `#`
    /// heading produced no code at all while the identical text in a
    /// paragraph produced one.
    #[test]
    fn a_refused_shape_in_a_heading_is_reported_too() {
        let out = extract_links("# a [[|alias]] heading", &[]);
        assert!(out.links.is_empty());
        assert_eq!(
            out.losses,
            [Degradation::UnknownLinkSyntax],
            "a refused shape is a loss wherever it appears, heading included"
        );
    }

    #[test]
    fn an_excluded_span_hides_its_links() {
        let text = "before [[a]] scene [[b]] after [[c]]";
        // Exclude the `[[b]]` occurrence wholesale.
        let start = text.find("[[b]]").expect("present");
        let out = extract_links(text, std::slice::from_ref(&(start..start + 5)));
        assert_eq!(
            out.links
                .iter()
                .map(|link| link.raw_target.as_str())
                .collect::<Vec<_>>(),
            ["a", "c"],
            "the scene's link is not a prose link"
        );
    }

    #[test]
    fn frontmatter_links_come_from_raw_with_comment_lines_skipped() {
        let raw = "tags: [x]\n# see [[hidden]]\nrelated: \"[[real]]\"\nbody: |\n  inside [[block]]";
        let out = extract_frontmatter_links(raw);
        let targets: Vec<&str> = out
            .links
            .iter()
            .map(|link| link.raw_target.as_str())
            .collect();
        assert_eq!(
            targets,
            ["real", "block"],
            "comments skipped, block content counted"
        );
        assert!(
            out.links
                .iter()
                .all(|link| link.link_source == LinkSource::Frontmatter)
        );
    }

    #[test]
    fn the_same_writing_twice_is_one_record() {
        let out = extract_links("[[x]] again [[x]]", &[]);
        assert_eq!(out.links.len(), 1, "identity dedupe, first occurrence wins");
    }

    #[test]
    fn an_outline_shares_the_chunkers_heading_semantics() {
        let text = "# A\n\ntext\n\n## B\n\n^id1\n\n- item ^id2\n";
        let outline = outline_of(text);
        assert_eq!(
            outline.headings,
            [vec!["A".to_owned()], vec!["A".to_owned(), "B".to_owned()]]
        );
        assert_eq!(outline.block_ids, ["id1", "id2"]);
        // Real cross-check: every heading path the chunker derives from the
        // same text is a path the outline recorded — two implementations of the
        // h1-h6 stack, one witness.
        let policy: momotaro_contracts::IndexPolicy =
            serde_json::from_str("{}").expect("policy defaults");
        for chunk in crate::chunk_markdown_with(
            "note:x.md",
            "h",
            text,
            &policy,
            crate::chunker::SourceOffset::default(),
            &[],
            None,
        ) {
            if let Some(path) = &chunk.heading_path {
                let levels: Vec<String> = path.split(" > ").map(str::to_owned).collect();
                assert!(
                    outline.headings.contains(&levels),
                    "chunk path {path} missing from the outline"
                );
            }
        }
    }
    #[test]
    fn a_heading_anchor_matches_by_tail_text() {
        let outline = outline_of("# Parent\n\n## Child\n\ntext\n");
        assert!(anchor_still_exists(AnchorKind::Heading, "Child", &outline));
        assert!(anchor_still_exists(
            AnchorKind::Heading,
            "Parent#Child",
            &outline
        ));
        assert!(!anchor_still_exists(
            AnchorKind::Heading,
            "Missing",
            &outline
        ));
        assert!(
            !anchor_still_exists(AnchorKind::Heading, "Child#Parent", &outline),
            "levels are ordered: the tail must match"
        );

        // The empty-level guard is redundant *because of this*: an empty
        // heading level never reaches an outline (the builder filters empty
        // stack slots), so no path could ever match an empty level and the
        // guard has nothing left to refuse. Pin the filter — if it goes, the
        // guard becomes load-bearing again.
        let with_empty = outline_of(
            "#

## Child

text
",
        );
        assert!(
            with_empty
                .headings
                .iter()
                .all(|path| path.iter().all(|level| !level.is_empty())),
            "an empty heading level never reaches an outline: {:?}",
            with_empty.headings
        );
        assert!(!anchor_still_exists(AnchorKind::Heading, "", &with_empty));
    }

    /// The one place a markdown target is decoded — the extraction side
    /// deliberately stores it raw, and its own witness says "decoding is
    /// resolution's job". This is that job: a `%20` in a markdown link has to
    /// find the file whose name holds a space, and a wikilink target has to
    /// stay exactly as written.
    #[test]
    fn a_markdown_target_is_decoded_here_and_a_wikilink_is_not() {
        let ns = Namespace::new([("my note.md", "note:my note.md")]);

        assert_eq!(
            resolve("my%20note.md", "", LinkSource::Markdown, &ns),
            Resolution::Resolved("note:my note.md".to_owned()),
            "the markdown half decodes, so the link finds its file"
        );
        assert!(
            matches!(
                resolve("my%20note.md", "", LinkSource::Wikilink, &ns),
                Resolution::Dangling
            ),
            "a wikilink is never decoded — `my%20note.md` is its own name, \
             and decoding it would resolve a target nobody wrote"
        );
    }

    /// Inline code is heading text for the outline too: the anchor a reader
    /// writes matches the rendered spelling (`#Foo bar baz`), or a link to a
    /// heading that is on the page is reported stale — and the row lands in
    /// the audit log as an error that never happened.
    #[test]
    fn inline_code_is_heading_text_for_the_outline() {
        let outline = outline_of("# Foo `bar` baz\n\ntext\n");
        assert!(
            anchor_still_exists(AnchorKind::Heading, "Foo bar baz", &outline),
            "the rendered spelling matches: {outline:?}"
        );
    }

    #[test]
    fn a_block_anchor_matches_a_definition() {
        let outline = outline_of("para ^abc\n");
        assert!(anchor_still_exists(AnchorKind::Block, "^abc", &outline));
        assert!(!anchor_still_exists(AnchorKind::Block, "^nope", &outline));
    }

    #[test]
    fn resolution_prefers_the_links_own_directory() {
        let ns = Namespace::new([
            ("a/note.md", "note:a/note.md"),
            ("b/note.md", "note:b/note.md"),
        ]);
        assert_eq!(
            resolve("note", "a", LinkSource::Wikilink, &ns),
            Resolution::Resolved("note:a/note.md".to_owned())
        );
        assert_eq!(
            resolve("note", "c", LinkSource::Wikilink, &ns),
            Resolution::Ambiguous(vec![
                "note:a/note.md".to_owned(),
                "note:b/note.md".to_owned()
            ]),
            "no directory preference means no edge, and the candidates are listed"
        );
    }

    #[test]
    fn resolution_matches_paths_and_stems() {
        let ns = Namespace::new([("docs/x.md", "note:docs/x.md"), ("img.png", "note:img.png")]);
        assert_eq!(
            resolve("docs/x", "docs", LinkSource::Wikilink, &ns),
            Resolution::Resolved("note:docs/x.md".to_owned()),
            "a path form appends .md"
        );
        assert_eq!(
            resolve("docs/x.md", "docs", LinkSource::Markdown, &ns),
            Resolution::Resolved("note:docs/x.md".to_owned()),
            "and matches exactly when spelled in full"
        );
        assert_eq!(
            resolve("img", "", LinkSource::Wikilink, &ns),
            Resolution::Resolved("note:img.png".to_owned()),
            "a bare name matches a stem across extensions"
        );
        assert_eq!(
            resolve("gone", "", LinkSource::Wikilink, &ns),
            Resolution::Dangling
        );
    }

    #[test]
    fn a_relative_escape_never_becomes_a_key() {
        let ns = Namespace::new([("docs/x.md", "note:docs/x.md")]);
        assert_eq!(
            resolve("../secret", "docs", LinkSource::Markdown, &ns),
            Resolution::Dangling,
            "`..` never resolves (the key layer refuses it too)"
        );
        assert_eq!(
            resolve("./docs/x.md", "", LinkSource::Markdown, &ns),
            Resolution::Resolved("note:docs/x.md".to_owned()),
            "`./` is stripped before matching"
        );
    }

    #[test]
    fn a_multibyte_character_before_brackets_does_not_panic() {
        // A regression: the embed check sliced at `open - 1`, which is not a
        // char boundary when a wide character precedes `[[` — one CJK line in
        // frontmatter took down the whole index run.
        let out = extract_frontmatter_links("related: \u{4e2d}[[target]]");
        let targets: Vec<&str> = out
            .links
            .iter()
            .map(|link| link.raw_target.as_str())
            .collect();
        assert_eq!(targets, ["target"]);
        let out = extract_frontmatter_links("related: \u{4e2d}![[target]]");
        assert_eq!(
            out.links.first().map(|link| link.edge_kind),
            Some(EdgeKind::Embed),
            "the embed check still fires after a wide character"
        );
    }

    #[test]
    fn losses_come_in_declaration_order_not_first_occurrence() {
        // The blob's `link_losses` is documented as declaration order; a
        // remnant found before a bad markdown link must not flip the list.
        let out = extract_links("a [[|x]] b [t](bad%ZZ.md)", &[]);
        assert_eq!(
            out.losses,
            [
                Degradation::MarkdownLinkDecodeError,
                Degradation::UnknownLinkSyntax
            ],
            "sorted into the enum's order, whichever came first in the text"
        );
    }

    #[test]
    fn an_excluded_span_does_not_stitch_a_remnant_together() {
        let text = "a [[ b SKIPPED c ]] d";
        let start = text.find("SKIPPED").expect("present");
        let out = extract_links(text, std::slice::from_ref(&(start..start + 7)));
        assert!(
            out.losses.is_empty(),
            "the brackets belong to different runs; no remnant exists: {:?}",
            out.losses
        );
    }

    #[test]
    fn block_ids_are_not_read_out_of_code_fences() {
        let outline = outline_of("```\n^infence\n```\n\npara ^real\n");
        assert_eq!(
            outline.block_ids,
            ["real"],
            "inside a fence it is code someone wrote, not a definition"
        );
    }

    #[test]
    fn matching_follows_nfc_but_never_case_folds() {
        // D42: comparison follows the key's normalization (the raw writing
        // never has to be normalized by its author), and case is identity —
        // folding two distinct Linux files into one match is data loss.
        let ns = Namespace::new([("caf\u{e9}.md", "note:caf\u{e9}.md")]);
        assert_eq!(
            resolve("cafe\u{301}.md", "", LinkSource::Wikilink, &ns),
            Resolution::Resolved("note:caf\u{e9}.md".to_owned()),
            "an NFD writing matches the NFC key"
        );
        let ns = Namespace::new([("Note.md", "note:Note.md")]);
        assert_eq!(
            resolve("note.md", "", LinkSource::Wikilink, &ns),
            Resolution::Dangling,
            "case is never folded"
        );
    }

    /// `End(CodeBlock)` is what puts the text run back in play. Latch the flag
    /// and every paragraph after the first fence is read as code, so a refused
    /// `[[…]]` written in prose never reaches the scan — a loss `§5 第 6 条`
    /// names by shape and this doc's own fence test would have you believe is
    /// quiet forever.
    #[test]
    fn a_refused_shape_after_a_code_block_is_reported_too() {
        let out = extract_links("```\ncode\n```\n\na [[|alias]] b", &[]);
        assert!(out.links.is_empty());
        assert_eq!(
            out.losses,
            [Degradation::UnknownLinkSyntax],
            "the fence is over; what follows is prose again: {:?}",
            out.losses
        );
    }

    /// A block end closes the run, so an opener in one paragraph and its closer
    /// in the next are two halves of nothing — stitching them invents a remnant
    /// nobody wrote and reports a loss that did not happen.
    #[test]
    fn the_run_does_not_stitch_across_a_block_boundary() {
        let out = extract_links("[[\n\ntext]]", &[]);
        assert!(
            out.losses.is_empty(),
            "the brackets belong to different paragraphs: {:?}",
            out.losses
        );
    }

    /// Both halves of the caret-id rule (§5 第 4 条): whitespace before the `^`,
    /// and an id the grammar admits. Relax either and `outline.block_ids`
    /// grows a definition that was never written, so `anchor_still_exists`
    /// answers "still there" for an anchor that is gone (stale-anchor miss).
    #[test]
    fn a_caret_id_needs_whitespace_before_it_and_a_legal_id_after() {
        let outline = outline_of("abc^def\n\npara ^!!!\n\nok ^real\n");
        assert_eq!(
            outline.block_ids,
            ["real"],
            "no whitespace before the caret, and `!!!` is not an id: {:?}",
            outline.block_ids
        );
    }

    /// `is_block_id` is `[A-Za-z0-9-]+` and says so; a hyphen is the commonest
    /// thing in an id. Reject it and `^my-id` stops being a definition, so a
    /// link to it is reported stale while it is still on the page.
    #[test]
    fn a_block_id_may_contain_a_hyphen() {
        let outline = outline_of("para ^my-id\n");
        assert_eq!(outline.block_ids, ["my-id"], "{:?}", outline.block_ids);
    }

    /// Half-open on both sides, checked against the event that actually carries
    /// the link: a span that *begins* on its end byte, or *stops* on its start
    /// byte, does not contain it. `<=` on either operand swallows the link event
    /// and the edge vanishes — the span excluded nothing anybody wrote.
    ///
    /// The range has to come from the parse, not from a guess at where `[..]`
    /// sits: `analyze` tests every event separately, so a span adjacent to the
    /// paragraph says nothing about the link inside it.
    #[test]
    fn a_span_that_touches_an_event_without_overlapping_it_hides_nothing() {
        let text = "before [a](a.md) after";
        let mut link_ranges: Vec<Range<usize>> = Parser::new_ext(text, crate::PARSE_OPTIONS)
            .into_offset_iter()
            .filter(|(event, _)| matches!(event, Event::Start(Tag::Link { .. })))
            .map(|(_, range)| range)
            .collect();
        assert_eq!(link_ranges.len(), 1, "{link_ranges:?}");
        let range = link_ranges.remove(0);

        let targets = |excluded: &[Range<usize>]| -> Vec<String> {
            extract_links(text, excluded)
                .links
                .iter()
                .map(|link| link.raw_target.clone())
                .collect()
        };

        // Opens exactly on the link's end byte: covers what follows it only.
        assert_eq!(
            targets(std::slice::from_ref(&(range.end..text.len()))),
            ["a.md".to_owned()],
            "a span starting at an event's end does not reach into it: {range:?}"
        );
        // Stops exactly on the link's start byte: covers what precedes it only.
        assert_eq!(
            targets(std::slice::from_ref(&(0..range.start))),
            ["a.md".to_owned()],
            "a span ending at an event's start does not reach into it: {range:?}"
        );
    }

    /// Indented code ends *after* its last newline, which is exactly where the
    /// next line starts. The predicate has to keep those apart: read the line
    /// as code and its `^id` definition is swallowed by a fence it never entered.
    #[test]
    fn a_line_starting_exactly_where_indented_code_ends_is_not_code() {
        let outline = outline_of("    indented code\nb ^z\n");
        assert_eq!(
            outline.block_ids,
            ["z"],
            "one line past the fence is still prose: {:?}",
            outline.block_ids
        );
    }

    #[test]
    fn percent_decode_is_all_or_nothing() {
        assert_eq!(percent_decode("a%20b").as_deref(), Some("a b"));
        assert_eq!(percent_decode("%E4%B8%AD").as_deref(), Some("中"));
        // Lowercase is the same alphabet: `[t](%e4%b8%ad.md)` must build the
        // edge its uppercase twin builds, not a decode loss nobody wrote.
        assert_eq!(percent_decode("%e4%b8%ad").as_deref(), Some("中"));
        assert_eq!(percent_decode("%c3%a9").as_deref(), Some("\u{e9}"));
        assert_eq!(percent_decode("bad%ZZ"), None);
        assert_eq!(percent_decode("trail%"), None);
        assert_eq!(percent_decode("%FF"), None, "valid escape, invalid UTF-8");
    }
    /// The allocation-free `is_external` folds case per compared byte
    /// (`eq_ignore_ascii_case`); the old whole-string lowercase folded it
    /// wholesale. Uppercase spellings are the shapes the rewrite could have
    /// broken — this pins them (probe P1-F2).
    #[test]
    fn an_uppercase_uri_scheme_is_still_external() {
        assert!(is_external("MAILTO:someone@example.com"));
        assert!(is_external("HTTPS://example.com/x.md"));
        assert!(is_external("Http://example.com"));
        assert!(!is_external("arxiv:1234"));
        assert!(!is_external("note.md"));
    }
}
