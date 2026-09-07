//! Locating a rendered selection inside a transcript.
//!
//! Everything happens in the dense view built by [`crate::normalize`], so line
//! wrapping, gutters and Markdown markers are already out of the way. What is
//! left is a substring search plus two tolerances: a selection can clip a
//! character at either edge, and the renderer can truncate long output in the
//! middle. The result is always a slice of the untouched transcript, which is
//! why intentional newlines, indentation and Unicode come back intact without
//! any special handling.

use crate::codex::{Block, BlockKind};
use crate::normalize::DenseIndex;
use memchr::memmem;

/// How a match was found, reported so a surprising result can be explained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The selection appears verbatim in the transcript.
    Exact,
    /// It appears once a stray character is ignored at one or both edges.
    Trimmed,
    /// Only the head and tail could be located, so the span between them was
    /// taken from the transcript. This is what survives a status row drawn over
    /// the middle of the selection.
    Anchored,
}

impl Strategy {
    pub fn label(self) -> &'static str {
        match self {
            Strategy::Exact => "exact match",
            Strategy::Trimmed => "trimmed match",
            Strategy::Anchored => "anchored match",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Match {
    pub text: String,
    pub strategy: Strategy,
    /// Number of places the selection could have come from. More than one means
    /// the most recent was chosen.
    pub candidates: usize,
    pub block: Option<BlockKind>,
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Characters that may be discarded from each edge of the selection.
    pub max_trim: usize,
    /// Characters used as the head and tail anchors of the last-resort search.
    pub anchor_len: usize,
    /// Shortest span worth trusting. Below this, a match says nothing: a couple
    /// of characters turn up somewhere in any transcript.
    pub min_span: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_trim: 3,
            anchor_len: 12,
            min_span: 4,
        }
    }
}

/// A transcript flattened into one searchable body of text.
///
/// Blocks are concatenated rather than searched one by one so that a selection
/// running from the end of one message into the next still has somewhere to
/// match, and so a single search covers every block.
#[derive(Debug)]
pub struct Haystack {
    index: DenseIndex,
    spans: Vec<Span>,
}

#[derive(Debug)]
struct Span {
    kind: BlockKind,
    start: usize,
    end: usize,
}

const BLOCK_SEPARATOR: &str = "\n\n";

impl Haystack {
    pub fn build(blocks: &[Block]) -> Self {
        let mut source = String::new();
        let mut bounds = Vec::with_capacity(blocks.len());
        for block in blocks {
            if !source.is_empty() {
                source.push_str(BLOCK_SEPARATOR);
            }
            let start = source.len();
            source.push_str(&block.text);
            bounds.push((block.kind, start, source.len()));
        }

        let index = DenseIndex::build(&source);
        // Byte bounds become grapheme bounds by counting how many retained
        // graphemes start before each block.
        let spans = bounds
            .into_iter()
            .map(|(kind, start, end)| Span {
                kind,
                start: index.graphemes_before(start),
                end: index.graphemes_before(end),
            })
            .collect();

        Haystack { index, spans }
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn block_at(&self, grapheme: usize) -> Option<BlockKind> {
        self.spans
            .iter()
            .find(|span| grapheme >= span.start && grapheme < span.end)
            .map(|span| span.kind)
    }

    fn finish(&self, start: usize, end: usize, strategy: Strategy, candidates: usize) -> Match {
        Match {
            text: self.index.slice(start, end).to_string(),
            strategy,
            candidates,
            block: self.block_at(start),
        }
    }
}

/// Find `selection` inside `haystack`, returning the original transcript text.
pub fn find(haystack: &Haystack, selection: &DenseIndex, options: Options) -> Option<Match> {
    if haystack.is_empty() || selection.is_empty() {
        return None;
    }

    if let Some(found) = find_exact(haystack, selection.dense()) {
        return Some(haystack.finish(found.start, found.end, Strategy::Exact, found.candidates));
    }
    if let Some(found) = find_trimmed(haystack, selection, options) {
        return Some(haystack.finish(found.start, found.end, Strategy::Trimmed, found.candidates));
    }
    if let Some(found) = find_anchored(haystack, selection, options) {
        return Some(haystack.finish(found.start, found.end, Strategy::Anchored, found.candidates));
    }
    None
}

struct Found {
    start: usize,
    end: usize,
    candidates: usize,
}

/// Every position where `needle` occurs on grapheme boundaries, as grapheme
/// ranges into the haystack, in order.
///
/// Overlapping occurrences are included. `memmem::find_iter` skips them, which
/// would both undercount the candidates and hide the most recent occurrence —
/// the one this program deliberately prefers.
fn occurrences(haystack: &Haystack, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = haystack.index.dense().as_bytes();
    let finder = memmem::Finder::new(needle.as_bytes());
    let mut found = Vec::new();
    let mut from = 0;
    while from < hay.len() {
        let Some(offset) = finder.find(&hay[from..]) else {
            break;
        };
        let at = from + offset;
        // A byte offset can land inside a character; those are not matches.
        if let (Some(start), Some(end)) = (
            haystack.index.grapheme_at(at),
            haystack.index.grapheme_at(at + needle.len()),
        ) {
            found.push((start, end));
        }
        from = at + 1;
    }
    found
}

/// The last occurrence wins: when the same words appear more than once, the one
/// furthest down the transcript is the one most likely still on screen.
fn find_exact(haystack: &Haystack, needle: &str) -> Option<Found> {
    let hits = occurrences(haystack, needle);
    let (start, end) = *hits.last()?;
    Some(Found {
        start,
        end,
        candidates: hits.len(),
    })
}

/// Retry with a few characters shaved off each edge. A drag can pick up a
/// character the transcript has no counterpart for — a fragment of the row
/// below, part of a neighbouring pane — while everything between the edges is
/// intact.
///
/// Only the span that was actually located is returned. Putting the discarded
/// characters back would mean handing over text that was never verified, which
/// is the guesswork this tool exists to avoid.
fn find_trimmed(haystack: &Haystack, selection: &DenseIndex, options: Options) -> Option<Found> {
    let total = selection.len();
    // Ordered by how much is being discarded, so the closest reading wins.
    let mut budgets: Vec<(usize, usize)> = (0..=options.max_trim)
        .flat_map(|head| (0..=options.max_trim).map(move |tail| (head, tail)))
        .filter(|&(head, tail)| head + tail > 0)
        .collect();
    budgets.sort_by_key(|&(head, tail)| (head + tail, head));

    // Discarding an edge is only a minor correction when what remains still
    // dominates the selection. Without this, a six character selection could be
    // pared down to two and "verified" against a coincidence.
    let floor = options.min_span.max(total / 2);

    for (head, tail) in budgets {
        if total.saturating_sub(head + tail) < floor {
            continue;
        }
        let trimmed = selection.dense_slice(head, total - tail);
        let hits = occurrences(haystack, trimmed);
        if let Some(&(start, end)) = hits.last() {
            return Some(Found {
                start,
                end,
                candidates: hits.len(),
            });
        }
    }
    None
}

/// Locate the head and tail separately and keep the transcript text between
/// them, for selections whose middle has no direct counterpart.
///
/// That happens in both directions. The renderer rewrites some things rather
/// than merely styling them — a Markdown link becomes a short relative path, so
/// the transcript holds *more* characters than were on screen. And a drag can
/// cross a row the TUI drew over the transcript, a spinner or a status line,
/// putting characters in the selection that belong to no message at all.
///
/// The two are bounded separately. Growth is allowed generously because it comes
/// from text that was always there; shrinkage is kept tight because it means
/// trusting a span shorter than what was selected. Either way the span has to
/// stay near the length of the selection, so a head anchor that also appears far
/// away cannot drag in a large stretch of unrelated transcript.
fn find_anchored(haystack: &Haystack, selection: &DenseIndex, options: Options) -> Option<Found> {
    let total = selection.len();
    if total < options.anchor_len * 2 {
        return None;
    }
    let head = occurrences(haystack, selection.dense_slice(0, options.anchor_len));
    let tail = occurrences(
        haystack,
        selection.dense_slice(total - options.anchor_len, total),
    );
    if head.is_empty() || tail.is_empty() {
        return None;
    }

    let longest = total + total / 2 + 16;
    let shortest = total - (total / 4 + 8).min(total);
    let mut best: Option<(usize, usize, usize)> = None;
    for &(start, _) in &head {
        for &(_, end) in &tail {
            if end <= start {
                continue;
            }
            let span = end - start;
            if span > longest || span < shortest {
                continue;
            }
            let distance = span.abs_diff(total);
            if best.is_none_or(|(best_distance, ..)| distance < best_distance) {
                best = Some((distance, start, end));
            }
        }
    }

    let (_, start, end) = best?;
    Some(Found {
        start,
        end,
        candidates: head.len().max(tail.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haystack(texts: &[&str]) -> Haystack {
        let blocks: Vec<Block> = texts
            .iter()
            .map(|text| Block {
                kind: BlockKind::AgentMessage,
                text: (*text).to_string(),
            })
            .collect();
        Haystack::build(&blocks)
    }

    fn restore(texts: &[&str], selection: &str) -> Option<String> {
        let index = DenseIndex::from_rendered(selection);
        find(&haystack(texts), &index, Options::default()).map(|found| found.text)
    }

    /// Ordinary prose wrapped across rows, with the assistant gutter and the
    /// continuation indent that the TUI adds.
    #[test]
    fn joins_wrapped_prose_without_inserting_a_newline() {
        let source =
            "The deployment completed successfully, and the service is now running normally.";
        let selection =
            "• The deployment completed successfully, and the service is now\n  running normally.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    /// A newline the author meant is part of the transcript, so it survives.
    #[test]
    fn keeps_an_intentional_newline() {
        let source =
            "The migration completed successfully.\nThe next step is to restart the application.";
        let selection = "• The migration completed successfully.\n  The next step is to restart the\n  application.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn rejoins_a_word_split_by_wrapping() {
        let source = "The authentication middleware is configured correctly.";
        let selection = "• The authentica\n  tion middleware is configured correctly.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn does_not_break_a_line_wrapped_after_punctuation() {
        let source = "The request was accepted. Processing will continue in the background.";
        let selection =
            "• The request was accepted.\n  Processing will continue in the background.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn keeps_every_newline_of_multi_line_prose() {
        let source = "The server is healthy.\nThe database connection is healthy.\nAll background workers are running.";
        let selection = "• The server is healthy.\n  The database connection is\n  healthy.\n  All background workers are\n  running.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn preserves_code_indentation_exactly() {
        let source = "const config = {\n    retries: 3,\n    timeout: 5000,\n};";
        let selection = "  const config = {\n      retries: 3,\n      timeout: 5000,\n  };";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn keeps_a_long_shell_command_on_one_line() {
        let source = "git log --oneline --decorate --graph --all --max-count=50";
        let selection = "  git log --oneline --decorate --graph\n  --all --max-count=50";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn keeps_a_wrapped_url_in_one_piece() {
        let source = "https://example.com/docs/guides/configuration/advanced-options";
        let selection = "  https://example.com/docs/guides/co\n  nfiguration/advanced-options";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn returns_only_the_selected_part_of_a_line() {
        let source = "The deployment completed successfully.";
        assert_eq!(
            restore(&[source], "deployment completed").as_deref(),
            Some("deployment completed")
        );
    }

    #[test]
    fn returns_the_logical_span_of_a_partial_multi_row_selection() {
        let source = "The server is healthy.\nThe database connection is healthy.";
        let selection = "is healthy.\n  The database connection";
        assert_eq!(
            restore(&[source], selection).as_deref(),
            Some("is healthy.\nThe database connection")
        );
    }

    /// Codex strips Markdown markers while rendering, so the selection has fewer
    /// characters than the transcript. Both sides reduce to the same dense form.
    #[test]
    fn matches_through_stripped_markdown_markers() {
        let source = "The schema is confirmed. The target is `sample_items`, and the member count comes from `sample_members`. That is the setup.";
        let selection = "• The schema is confirmed. The target is\n  sample_items, and the member count\n  comes from sample_members. That is\n  the setup.";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    /// Non-space scripts wrap without consuming a space. The transcript settles
    /// it, so no rule about spaces is needed.
    #[test]
    fn joins_non_space_wraps_without_inserting_a_space() {
        let source =
            "\u{4E00}\u{4E8C}\u{4E09}\u{56DB}\u{4E94}\u{516D}\u{4E03}\u{516B}\u{4E5D}\u{5341}";
        let selection = "• \u{4E00}\u{4E8C}\u{4E09}\u{56DB}\n  \u{4E94}\u{516D}\u{4E03}\u{516B}\u{4E5D}\n  \u{5341}";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn ignores_blank_rows_the_renderer_inserted_between_list_items() {
        let source = "- 2 apples and 3 pears\n- `item_id` and `status = 'fresh'` identify the item\n- restock notes are required";
        let selection = "  - 2 apples and 3 pears\n  - item_id and status =\n    'fresh' identify the item\n\n  - restock notes are required";
        assert_eq!(restore(&[source], selection).as_deref(), Some(source));
    }

    #[test]
    fn keeps_emoji_intact() {
        let source = "Schema check \u{1F4A1}\u{2728} is complete and ready.";
        let selection = "• Schema check \u{1F4A1}\u{2728} is complete\n  and ready.";
        let restored = restore(&[source], selection);
        assert_eq!(restored.as_deref(), Some(source));
        assert!(restored.unwrap().contains('\u{1F4A1}'));
    }

    /// A drag that overshoots picks up characters belonging to another row. The
    /// verified span is returned; the strays are neither kept nor guessed back.
    #[test]
    fn ignores_stray_characters_at_the_selection_edges() {
        let source = "Deployment completed successfully.";
        let selection = "7ployment completed success+";
        let found = find(
            &haystack(&[source]),
            &DenseIndex::from_rendered(selection),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.text, "ployment completed success");
        assert_eq!(found.strategy, Strategy::Trimmed);
    }

    /// The TUI can draw a status row across the middle of a selection. Its text
    /// belongs to no message, so the transcript span between the ends is used.
    #[test]
    fn steps_over_a_status_row_drawn_through_the_selection() {
        let source = "Prepare the deployment. First inspect the configuration and update it if needed. Then apply it to the staging environment and verify the result.";
        let selection = "• Prepare the deployment. First\n  inspect the configuration and update it\n  if needed.\n▌ Working (12s)\n  Then apply it to the staging environment\n  and verify the result.";
        let found = find(
            &haystack(&[source]),
            &DenseIndex::from_rendered(selection),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.text, source);
        assert_eq!(found.strategy, Strategy::Anchored);
    }

    /// Codex renders a Markdown link as a path relative to the session
    /// directory, so the transcript holds a stretch of characters — the link
    /// text and the full path — that were never on screen. Recovering the link
    /// is useful when enough surrounding prose makes the span identifiable.
    ///
    /// The inserted path is a fixed size while the bound is proportional, so
    /// this only works for a selection with enough prose around the link. A
    /// short selection built almost entirely of a link is not recovered, which
    /// is the intended trade: at that size the anchors carry too little weight
    /// to justify trusting a span half again as long.
    #[test]
    fn recovers_a_paragraph_whose_link_the_renderer_shortened() {
        let source = "A deployment report links to the latest entry through [config.rs](/sample/src/config.rs:87). The visible path is shorter than the stored link, but the surrounding sentence supplies enough context to identify the source.";
        let selection = "  A deployment report links to the latest entry through config.rs:87. The visible path is\n  shorter than the stored link, but the surrounding sentence supplies enough context to\n  identify the source.";
        let found = find(
            &haystack(&[source]),
            &DenseIndex::from_rendered(selection),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.text, source);
        assert_eq!(found.strategy, Strategy::Anchored);
    }

    /// Growth is allowed generously, shrinkage is not: a span noticeably shorter
    /// than the selection would mean handing back less than was asked for.
    #[test]
    fn refuses_an_anchored_span_much_shorter_than_the_selection() {
        let source = "The first sentence. The final sentence.";
        // Nearly the whole selection is text the transcript has no counterpart
        // for, so the span between the anchors collapses to almost nothing.
        let selection =
            "The first sentence.".to_string() + &"UI_DECORATION".repeat(20) + "The final sentence.";
        assert_eq!(restore(&[source], &selection), None);
    }

    /// Both anchors are present but sit either side of a long unrelated block.
    /// Joining them would return a large stretch of transcript that was never
    /// selected, so nothing is returned and the caller falls back.
    #[test]
    fn refuses_an_anchored_span_far_longer_than_the_selection() {
        let blocks = [
            "Check the configuration. This is the first note.",
            "An unrelated long output block sits here. It was not selected and should not be restored as part of the result.",
            "Apply the change and finish. This is the final note.",
        ];
        // Head anchor lives in the first block, tail anchor in the last, and the
        // `XYZQW` in the middle keeps every exact and trimmed search from hitting.
        let selection =
            "Check the configuration. XYZQWApply the change and finish. This is the final note.";
        assert_eq!(restore(&blocks, selection), None);
    }

    /// The two occurrences reduce to the same dense form but sit in different
    /// surroundings, so the slice differs depending on which one is taken. That
    /// is what makes this test able to tell them apart at all.
    #[test]
    fn prefers_the_most_recent_of_several_identical_passages() {
        let found = find(
            &haystack(&["`deploy` done", "middle", "deploy done"]),
            &DenseIndex::build("deploydone"),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.text, "deploy done");
        assert_eq!(found.candidates, 2);
    }

    /// Overlapping occurrences count. Skipping them would report one candidate
    /// here and return the earlier of the two.
    #[test]
    fn counts_occurrences_that_overlap_each_other() {
        let found = find(
            &haystack(&["abab`ab"]),
            &DenseIndex::build("abab"),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.candidates, 2);
        assert_eq!(found.text, "ab`ab");
    }

    /// Same idea for the trimmed strategy, which has its own choice to make.
    #[test]
    fn a_trimmed_match_also_prefers_the_most_recent_occurrence() {
        let found = find(
            &haystack(&["`configuration` value", "middle", "configuration value"]),
            // The leading `%` has no counterpart, forcing a trim.
            &DenseIndex::build("%configurationvalue"),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.strategy, Strategy::Trimmed);
        assert_eq!(found.text, "configuration value");
    }

    /// Trimming is for a stray character or two. Paring a short selection down
    /// to a fragment would match something in any transcript, so it is refused
    /// rather than reported as verified.
    #[test]
    fn refuses_to_trim_a_short_selection_down_to_a_fragment() {
        // `ab` alone would be found, but it is a third of what was selected.
        assert_eq!(restore(&["about the configuration."], "QQabWW"), None);
    }

    #[test]
    fn still_trims_when_what_remains_dominates_the_selection() {
        let source = "Deployment completed successfully.";
        assert_eq!(
            restore(&[source], "7ployment completed success+").as_deref(),
            Some("ployment completed success")
        );
    }

    #[test]
    fn reports_the_block_a_match_came_from() {
        let blocks = vec![
            Block {
                kind: BlockKind::UserMessage,
                text: "Inspect the deployment group".into(),
            },
            Block {
                kind: BlockKind::Command,
                text: "psql -c 'select 1'".into(),
            },
        ];
        let found = find(
            &Haystack::build(&blocks),
            &DenseIndex::build("psql -c 'select 1'"),
            Options::default(),
        )
        .unwrap();
        assert_eq!(found.block, Some(BlockKind::Command));
    }

    #[test]
    fn returns_nothing_when_the_selection_is_not_in_the_transcript() {
        assert_eq!(
            restore(&["the transcript text"], "some unrelated text"),
            None
        );
    }

    #[test]
    fn returns_nothing_for_an_empty_transcript_or_selection() {
        assert_eq!(restore(&[], "anything"), None);
        assert_eq!(restore(&["anything"], "   \n  "), None);
    }
}
