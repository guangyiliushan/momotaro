//! `.canvas` files: the shape this crate recognises, and what it leaves to others.
//!
//! A `.canvas` file is JSON Canvas 1.0, not Markdown: it has no `---` block and no
//! headings, and its text lives in per-node fields the spec gives a text meaning
//! (`text`, `file` + `subpath`, `url`, `label`). Only the *shape* is recognised
//! here — the file name says it is a canvas — and the JSON itself is read in
//! `momotaro-run`, which already speaks JSON. This crate's dependency list stays
//! as it is, the same split [`crate::frontmatter`] uses for Obsidian's JSON
//! property blocks.
//!
//! What a canvas contributes to the index: nodes in array order (array order
//! is the z-index), then edges as one `<from text> -> <to text>` line
//! each, with `id`, geometry and colors kept out of the text.

use std::path::Path;

/// Whether a path names a JSON Canvas file.
///
/// Extension only, case-insensitively. The walker's whitelist and the indexing
/// path's branch share this one predicate, so a file cannot be walked in as a
/// canvas and then read as Markdown somewhere else.
#[must_use]
pub(crate) fn is_canvas_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("canvas"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_decides_case_insensitively() {
        assert!(is_canvas_path(Path::new("board.canvas")));
        assert!(is_canvas_path(Path::new("deep/nested/Board.CANVAS")));
        assert!(!is_canvas_path(Path::new("note.md")));
        // A `.canvas` is a whole file, not a suffix: these are Markdown notes.
        assert!(!is_canvas_path(Path::new("board.canvas.md")));
        assert!(!is_canvas_path(Path::new("canvas")));
        assert!(!is_canvas_path(Path::new("board.canvasx")));
    }
}
