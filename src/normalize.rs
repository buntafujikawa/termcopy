//! The dense view: text stripped down to the characters that survive rendering,
//! plus a map back into the original bytes.
//!
//! Matching a terminal selection against a transcript cannot compare the two
//! directly. The renderer inserts line breaks at the pane width, prefixes rows
//! with gutters and indentation, and drops Markdown markers. So both sides are
//! reduced to the same "dense" form — no whitespace, no markers, no decoration —
//! and compared there. Because the reduction is identical on both sides, being
//! aggressive about what to drop can only cost discriminating power, never
//! correctness: the answer is always sliced out of the untouched original.

use unicode_segmentation::UnicodeSegmentation;

/// Text alongside its dense form and a grapheme-level map between the two.
#[derive(Debug, Clone, Default)]
pub struct DenseIndex {
    source: String,
    dense: String,
    /// For each retained grapheme: where it starts in `dense`.
    dense_offsets: Vec<usize>,
    /// For each retained grapheme: the byte range it occupies in `source`.
    source_starts: Vec<usize>,
    source_ends: Vec<usize>,
}

/// Strip the line-number gutter Codex draws down the left of a diff.
///
/// The numbers belong to the renderer, not the file, so a selection taken from
/// a diff carries digits the transcript has no counterpart for:
///
/// ```text
///     187 +            if [ "$old_mode" != "$current_mode" ]; then
/// ```
///
/// Only `123 +` and `123 -` count as a gutter. A bare leading number is left
/// alone — it is far more likely to be content, and removing it would take the
/// number off the front of whatever came back.
pub fn strip_diff_line_numbers(rendered: &str) -> String {
    let mut out = String::with_capacity(rendered.len());
    for line in rendered.split_inclusive('\n') {
        let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
        let (blank, rest) = line.split_at(indent);
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let is_gutter = digits > 0
            && rest[digits..].starts_with(' ')
            && rest[digits + 1..].starts_with(['+', '-']);

        out.push_str(blank);
        out.push_str(if is_gutter { &rest[digits..] } else { rest });
    }
    out
}

impl DenseIndex {
    /// Build from text as the terminal drew it, removing what the renderer put
    /// there. Use this for a selection; the transcript side wants [`build`].
    ///
    /// [`build`]: DenseIndex::build
    pub fn from_rendered(rendered: &str) -> Self {
        Self::build(&strip_diff_line_numbers(rendered))
    }

    pub fn build(source: &str) -> Self {
        let mut index = DenseIndex {
            source: source.to_string(),
            ..Default::default()
        };
        for (offset, grapheme) in source.grapheme_indices(true) {
            if grapheme.chars().all(is_droppable) {
                continue;
            }
            index.dense_offsets.push(index.dense.len());
            index.source_starts.push(offset);
            index.source_ends.push(offset + grapheme.len());
            index.dense.push_str(grapheme);
        }
        index
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn dense(&self) -> &str {
        &self.dense
    }

    /// Number of retained graphemes.
    pub fn len(&self) -> usize {
        self.dense_offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.dense_offsets.is_empty()
    }

    /// Byte offset in `dense` where the given grapheme starts.
    pub fn dense_offset(&self, grapheme: usize) -> Option<usize> {
        self.dense_offsets.get(grapheme).copied()
    }

    /// Grapheme position of a byte offset in `dense`, or `None` if the offset
    /// falls inside a grapheme. Substring searches work on bytes and can land
    /// mid-grapheme, which must never be treated as a match.
    pub fn grapheme_at(&self, dense_byte: usize) -> Option<usize> {
        if dense_byte == self.dense.len() {
            return Some(self.dense_offsets.len());
        }
        self.dense_offsets.binary_search(&dense_byte).ok()
    }

    /// How many retained graphemes start before a byte offset in `source`.
    /// Converts positions expressed in the original text into dense positions.
    pub fn graphemes_before(&self, source_byte: usize) -> usize {
        self.source_starts
            .partition_point(|&start| start < source_byte)
    }

    /// The original text spanned by graphemes `start..end`, including any
    /// whitespace and markers that were dropped between them.
    pub fn slice(&self, start: usize, end: usize) -> &str {
        if start >= end || end > self.source_starts.len() {
            return "";
        }
        &self.source[self.source_starts[start]..self.source_ends[end - 1]]
    }

    /// The dense substring for graphemes `start..end`.
    pub fn dense_slice(&self, start: usize, end: usize) -> &str {
        if start >= end || end > self.dense_offsets.len() {
            return "";
        }
        let from = self.dense_offsets[start];
        let to = self
            .dense_offsets
            .get(end)
            .copied()
            .unwrap_or(self.dense.len());
        &self.dense[from..to]
    }
}

/// Characters that carry no signal because the renderer adds, removes or
/// reflows them.
fn is_droppable(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_control() {
        return true;
    }
    match ch {
        // Markdown markers, which the TUI renders as styling rather than text.
        '`' | '*' | '_' | '~' | '#' => true,
        // Gutters and bullets the TUI draws beside content.
        '•' | '·' | '›' | '‹' | '▌' | '⏺' | '↳' | '⎿' => true,
        // Truncation and elision markers, and the glyph a clipped wide character
        // leaves behind.
        '…' | '⋮' | '\u{FFFD}' => true,
        // Box drawing and block elements used for frames, rules and tables.
        '\u{2500}'..='\u{259F}' => true,
        // Zero-width joiners and marks that vary with how text was captured.
        '\u{200B}'..='\u{200F}' | '\u{FEFF}' => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_wrapping_gutters_and_markdown_alike() {
        let rendered = DenseIndex::build(
            "• The schema is confirmed \u{2728}. The target is\n  sample_items, and the count is",
        );
        let source = DenseIndex::build(
            "The schema is confirmed \u{2728}. The target is `sample_items`, and the count is",
        );
        assert_eq!(rendered.dense(), source.dense());
    }

    #[test]
    fn slice_returns_original_bytes_including_dropped_ones() {
        let index = DenseIndex::build("The target is `sample_items`, and the count is");
        // The retained words span the code span, so its backticks come back.
        assert_eq!(
            index.slice(0, index.len()),
            "The target is `sample_items`, and the count is"
        );
    }

    #[test]
    fn slice_starts_and_ends_on_retained_graphemes() {
        let index = DenseIndex::build("  hello   world  ");
        assert_eq!(index.dense(), "helloworld");
        assert_eq!(index.slice(0, 5), "hello");
        assert_eq!(index.slice(5, 10), "world");
        assert_eq!(index.slice(0, 10), "hello   world");
    }

    #[test]
    fn keeps_emoji_and_combining_marks_whole() {
        let index = DenseIndex::build("go\u{1F4A1}\u{2728}cafe\u{0301}");
        assert_eq!(index.dense(), "go\u{1F4A1}\u{2728}cafe\u{0301}");
        assert_eq!(index.len(), 8);
        assert_eq!(index.slice(2, 4), "\u{1F4A1}\u{2728}");
    }

    #[test]
    fn rejects_byte_offsets_landing_inside_a_grapheme() {
        let index = DenseIndex::build("\u{4E00}\u{4E8C}\u{4E09}");
        assert_eq!(index.grapheme_at(0), Some(0));
        assert_eq!(index.grapheme_at(1), None);
        assert_eq!(index.grapheme_at(3), Some(1));
        assert_eq!(index.grapheme_at(9), Some(3));
        assert_eq!(index.grapheme_at(10), None);
    }

    #[test]
    fn empty_and_whitespace_only_input_is_empty() {
        assert!(DenseIndex::build("").is_empty());
        assert!(DenseIndex::build("   \n\t \u{3000}").is_empty());
    }

    #[test]
    fn removes_the_line_number_gutter_of_a_diff() {
        let rendered = "    187 +            if [ \"$old_mode\" ]; then\n    188 -    old\n";
        assert_eq!(
            strip_diff_line_numbers(rendered),
            "     +            if [ \"$old_mode\" ]; then\n     -    old\n"
        );
    }

    /// Numbers that are content keep their place. Dropping them would take the
    /// number off the front of whatever came back.
    #[test]
    fn leaves_leading_numbers_that_are_not_a_diff_gutter() {
        for line in [
            "42 files changed",
            "1. confirm the configuration",
            "2026-08-08 deployed",
            "    186",
            "187+no space",
        ] {
            assert_eq!(strip_diff_line_numbers(line), line, "{line:?}");
        }
    }

    #[test]
    fn a_diff_row_matches_the_patch_it_came_from() {
        let patch = DenseIndex::build("+            if [ \"$old_mode\" ]; then");
        let rendered = DenseIndex::from_rendered("    187 +            if [ \"$old_mode\" ]; then");
        assert_eq!(rendered.dense(), patch.dense());
    }

    #[test]
    fn keeps_list_markers_because_both_sides_render_them() {
        let index = DenseIndex::build("- restock notes are required");
        assert_eq!(index.dense(), "-restocknotesarerequired");
    }
}
