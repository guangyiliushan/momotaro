//! The Excalidraw plugin's drawing block: found by the plugin's own markers, kept
//! out of the index.
//!
//! `obsidian-excalidraw-plugin` stores a drawing inside an ordinary note: a fenced
//! code block under a `## Drawing` heading, wrapped in `%%` — Obsidian's comment
//! syntax, which hides it when the note is rendered and leaves every byte of it in
//! the file. That fenced block is the scene (compressed when the plugin's
//! compression is on): thousands of bytes of drawing data that reach the full-text
//! index today, while the note's real text sits right next to it under
//! `## Text Elements`.
//!
//! The rule below follows the plugin's own regexes (`DRAWING_COMPRESSED_REG` and
//! `DRAWING_REG` in `src/shared/excalidrawMarkdownParsing.ts`): a `##? Drawing`
//! heading at column 0, then — past anything that is not a backtick, blank lines
//! included (``[^`]*``) — the first fence, whose info string must be
//! `compressed-json` or `json`. Reading it that way is the point: a note with
//! this layout is one we cannot tell apart from the plugin's output, so the safe
//! move is to treat it as one. Where we read it more permissively than the plugin
//! might — we require no `%%` or `# Excalidraw Data` preamble — is deliberate.

use std::ops::Range;

/// The byte spans of a note's body that must not become chunk text.
///
/// Empty in the common case: a note that is not an Excalidraw drawing has no such
/// section, and detection is one pass with no allocation. The spans are in the
/// coordinates of `body` — the very text the chunker is handed — so nothing is
/// shifted and every chunk locator still addresses the note.
#[must_use]
pub fn scene_spans(body: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut after_drawing = false;
    while start < body.len() {
        let (line, next) = next_line(body, start);
        if after_drawing {
            // The plugin's regex allows anything but a backtick between the heading
            // and the fence (`[^`]*`), so blank lines, prose and `%%` are skipped —
            // and only the FIRST line carrying a backtick decides: either it opens
            // the scene, or the section is not the plugin's layout at all (another
            // language, an inline code span).
            if line.contains('`') {
                after_drawing = false;
                if let Some((info, ticks)) = fencing(line)
                    && matches!(info, "json" | "compressed-json")
                    && let Some(end) = closing_fence_end(body, next, ticks)
                {
                    spans.push(start..end);
                    start = end;
                    continue;
                }
            }
        } else if is_drawing_heading(line) {
            after_drawing = true;
        }
        start = next;
    }
    spans
}

/// The line at `start`, and the offset just past its newline (or the end of `body`).
fn next_line(body: &str, start: usize) -> (&str, usize) {
    let rest = &body[start..];
    match rest.find('\n') {
        Some(newline) => (&rest[..newline], start + newline + 1),
        None => (rest, body.len()),
    }
}

/// The plugin's section heading: column 0, nothing after it.
///
/// Its regex anchors on `\n##? Drawing\n`, so `## Drawing  ` with trailing
/// whitespace is a heading *we* would still read as one but not a layout the
/// plugin writes. Reading more permissively than the writer means quietly
/// dropping a hand-written note's content out of the index; being narrow here is
/// what keeps the two in step.
fn is_drawing_heading(line: &str) -> bool {
    line == "## Drawing" || line == "# Drawing"
}

/// A fence-opening line's info string and backtick count: at most three spaces of
/// indent, then three or more backticks, then what follows. `None` for any other
/// line. The count matters because a closing fence has to be at least as long.
fn fencing(line: &str) -> Option<(&str, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ticks = rest.len() - rest.trim_start_matches('`').len();
    if ticks < 3 {
        return None;
    }
    Some((rest[ticks..].trim(), ticks))
}

/// Just past the first line that closes the fence opened before `from`, if it has
/// one: nothing but backticks, and at least as many as the opener used — CommonMark's
/// own rule, so a shorter run inside the scene is content and not its end.
///
/// `None` when it is never closed: an unterminated block is not a layout the
/// plugin writes, and guessing where it would have ended would cut text the note
/// really has.
fn closing_fence_end(body: &str, from: usize, opener_ticks: usize) -> Option<usize> {
    let mut start = from;
    while start < body.len() {
        let (line, next) = next_line(body, start);
        let trimmed = line.trim_end();
        let indent = trimmed.len() - trimmed.trim_start_matches(' ').len();
        let ticks = &trimmed[indent..];
        if indent <= 3 && ticks.len() >= opener_ticks && ticks.bytes().all(|b| b == b'`') {
            return Some(start + trimmed.len());
        }
        start = next;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plugin's own layout, as `ExcalidrawData.ts` writes it (E1d): the section
    /// headings, the text elements, and the scene fence wrapped in `%%`.
    const DRAWING: &str = "\
---
excalidraw-plugin: parsed
tags: [excalidraw]
---

# Excalidraw Data

## Text Elements
Card onetoken

Card twotoken

%%
## Drawing
```compressed-json
N4KAkARALgngDgUwgLgAQQQDwMYEMA2AlgCYBOuA7hADTgQBuCpAzoQPYB2KqATLZMzYB
```
%%";

    #[test]
    fn the_scene_fence_is_the_span_and_the_text_elements_are_not() {
        let spans = scene_spans(DRAWING);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let scene = &DRAWING[spans[0].clone()];
        assert!(scene.starts_with("```compressed-json\n"), "{scene:?}");
        assert!(scene.ends_with("```"), "{scene:?}");
        // The fence and nothing else: what follows the span is the plugin's
        // trailing marker, and the text a user searches for is before it.
        assert_eq!(&DRAWING[spans[0].end..], "\n%%");
        let text = DRAWING.find("Card onetoken").expect("the text section");
        assert!(text < spans[0].start);
    }

    #[test]
    fn an_uncompressed_scene_is_the_same_shape() {
        let note = "## Drawing\n```json\n{\"type\":\"excalidraw\"}\n```\n%%\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(
            &note[spans[0].clone()],
            "```json\n{\"type\":\"excalidraw\"}\n```"
        );
    }

    #[test]
    fn a_json_fence_under_another_heading_is_left_alone() {
        let note = "## Notes\n```json\n{\"a\":1}\n```\n";
        assert!(scene_spans(note).is_empty());
    }

    #[test]
    fn the_first_fence_after_the_heading_decides() {
        // The plugin's regex cannot skip an earlier fence, so a section that opens
        // with some other language is not its layout — its scene would not be
        // found either.
        let note = "## Drawing\n```python\nprint(1)\n```\n```json\n{\"a\":1}\n```\n";
        assert!(scene_spans(note).is_empty());
    }

    #[test]
    fn an_unterminated_fence_is_left_alone() {
        let note = "## Drawing\n```compressed-json\nN4KAkARALg\n";
        assert!(scene_spans(note).is_empty());
    }

    #[test]
    fn every_drawing_section_is_found() {
        let note =
            "## Drawing\n```json\n{\"a\":1}\n```\n\ntext\n\n## Drawing\n```json\n{\"b\":2}\n```\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(&note[spans[0].clone()], "```json\n{\"a\":1}\n```");
        assert_eq!(&note[spans[1].clone()], "```json\n{\"b\":2}\n```");
    }

    #[test]
    fn an_indented_fence_is_the_same_fence() {
        // CommonMark allows up to three spaces of indent on a fence; four would be
        // an indented code block instead.
        let note = "## Drawing\n   ```json\n{\"a\":1}\n   ```\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!(note[spans[0].clone()].starts_with("   ```json"));

        let indented = "## Drawing\n    ```json\n{\"a\":1}\n    ```\n";
        assert!(scene_spans(indented).is_empty());
    }

    #[test]
    fn a_heading_with_trailing_space_is_not_the_plugins_heading() {
        // The plugin's regex anchors on `\n##? Drawing\n`. Accepting trailing
        // whitespace would make us read sections the plugin never writes, and every
        // extra one silently leaves a note's content out of the index.
        let note = "## Drawing  \n```compressed-json\nN4K\n```\n";
        assert!(scene_spans(note).is_empty());
        let exact = "## Drawing\n```compressed-json\nN4K\n```\n";
        assert_eq!(
            scene_spans(exact).len(),
            1,
            "the exact heading still counts"
        );
    }

    #[test]
    fn a_blank_line_between_the_heading_and_the_fence_still_counts() {
        // `[^`]*` in the plugin's regex: nothing but a backtick ends the gap, so a
        // note someone reformatted must not lose its scene to the index — that is
        // the one case this whole rule exists for.
        let note = "## Drawing\n\n```compressed-json\nBLANKSCENEtoken\n```\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!(note[spans[0].clone()].starts_with("```compressed-json"));
    }

    #[test]
    fn prose_and_the_comment_marker_between_them_are_skipped_too() {
        let note = "## Drawing\n%%\ntext with no backtick\n```json\n{\"a\":1}\n```\n";
        assert_eq!(scene_spans(note).len(), 1);
    }

    #[test]
    fn an_inline_backtick_before_the_fence_ends_the_section() {
        // The gap cannot cross a backtick the plugin's own regex would have stopped
        // at: prose mentioning code inline is not its layout.
        let note = "## Drawing\nuse `x` here\n```json\n{\"a\":1}\n```\n";
        assert!(scene_spans(note).is_empty());
    }

    #[test]
    fn a_shorter_backtick_run_inside_the_scene_is_content() {
        // CommonMark: the closing fence must be at least as long as the opener, so a
        // three-backtick line inside a four-backtick fence belongs to the scene.
        let note = "## Drawing\n````json\n{\"a\":1}\n```\n````\nAFTERtoken\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 1, "{spans:?}");
        let scene = &note[spans[0].clone()];
        assert!(
            scene.ends_with("````"),
            "the longer run closes it: {scene:?}"
        );
        assert!(
            scene.contains("```\n"),
            "the shorter one is content: {scene:?}"
        );
        assert!(note[spans[0].end..].contains("AFTERtoken"));
    }

    #[test]
    fn the_h1_form_is_the_same_section() {
        // `##?` in the plugin's regex: the h1 spelling is its layout too, and no
        // fixture used it before this test.
        let note = "# Notes\n\n# Drawing\n```json\n{\"a\":\"H1SCENEtoken\"}\n```\n\nH1AFTERtoken\n";
        let spans = scene_spans(note);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert!(note[spans[0].clone()].starts_with("```json"));
        assert!(
            note[spans[0].end..].contains("H1AFTERtoken"),
            "the text after it stays"
        );
    }
}
