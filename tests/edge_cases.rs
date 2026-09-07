//! Inputs that a clipboard can realistically hold, checked for panics.
//!
//! A selection is whatever the terminal gave the user, so it can be enormous,
//! empty, half a grapheme cluster or not text-like at all. None of that should
//! bring the program down.

use termcopy::align::{self, Haystack, Options};
use termcopy::codex::{Block, BlockKind};
use termcopy::heuristic;
use termcopy::normalize::DenseIndex;

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
    align::find(
        &haystack(texts),
        &DenseIndex::from_rendered(selection),
        Options::default(),
    )
    .map(|found| found.text)
}

/// Every prefix and suffix of a realistic transcript, which walks the slice
/// bounds across every grapheme boundary in both directions.
#[test]
fn every_substring_boundary_of_a_mixed_script_transcript_is_safe() {
    let source = "The target is `sample_groups` \u{1F4A1}\u{2728}\n    retries: 3,\nhttps://example.com/a/b?c=1#d\ncafe\u{0301} \u{4E00}\u{4E8C}";
    let hay = haystack(&[source]);
    let chars: Vec<char> = source.chars().collect();

    for end in 0..=chars.len() {
        for start in 0..=end {
            let selection: String = chars[start..end].iter().collect();
            // Only asserting that this returns rather than panics.
            let _ = align::find(
                &hay,
                &DenseIndex::from_rendered(&selection),
                Options::default(),
            );
            let _ = heuristic::rewrap(&selection, true);
            let _ = heuristic::rewrap(&selection, false);
        }
    }
}

#[test]
fn a_selection_longer_than_the_transcript_does_not_match() {
    let selection = "a very long selection ".repeat(200);
    assert_eq!(restore(&["short"], &selection), None);
}

#[test]
fn an_empty_transcript_matches_nothing() {
    assert_eq!(restore(&[], "anything at all"), None);
    assert_eq!(restore(&[""], "anything at all"), None);
    assert_eq!(restore(&["", "", ""], "anything at all"), None);
}

#[test]
fn a_selection_of_only_dropped_characters_matches_nothing() {
    // Every one of these is removed when building the dense view, so there is
    // nothing left to search for.
    for selection in [
        "   \n\t ",
        "\u{2022}\u{2022}\u{2022}",
        "```",
        "\u{3000}\u{200B}",
        "\u{2500}\u{2500}\u{2500}",
        "\u{2026}",
    ] {
        assert_eq!(
            restore(&["real transcript text"], selection),
            None,
            "{selection:?}"
        );
    }
}

#[test]
fn a_transcript_of_only_dropped_characters_matches_nothing() {
    assert_eq!(
        restore(
            &[
                "\u{2022}\u{2022}\u{2022}",
                "   ",
                "\u{2500}\u{2500}\u{2500}"
            ],
            "anything"
        ),
        None
    );
}

/// Combining marks and emoji sequences must come back whole rather than split
/// across the slice boundary.
#[test]
fn grapheme_clusters_survive_a_round_trip() {
    let source = "cafe\u{0301} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} family \u{1F1FA}\u{1F1F8} flag";
    let restored = restore(&[source], source).unwrap();
    assert_eq!(restored, source);
    assert!(restored.contains("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}"));
}

/// A drag can begin between the base character and its combining mark, leaving
/// a fragment the transcript has no counterpart for. The fragment is discarded
/// and the part that could be verified is returned, rather than reattaching a
/// base character the user never selected.
#[test]
fn a_selection_starting_inside_a_grapheme_cluster_drops_the_fragment() {
    let source = "caf\u{0301}eteria";
    assert_eq!(
        restore(&[source], "\u{0301}eteria").as_deref(),
        Some("eteria")
    );
}

#[test]
fn very_long_single_line_input_is_handled() {
    let source = "x".repeat(200_000);
    let restored = restore(&[&source], &source).unwrap();
    assert_eq!(restored.len(), 200_000);
}

#[test]
fn control_characters_and_lone_escapes_are_ignored_rather_than_matched() {
    let source = "plain transcript text";
    // Terminal captures can carry stray control bytes; they are dropped, so the
    // surrounding text still matches.
    assert_eq!(
        restore(&[source], "\u{1b}plain transcript\u{7} text\u{0}").as_deref(),
        Some(source)
    );
}

#[test]
fn the_fallback_never_panics_on_awkward_rows() {
    for selection in [
        "",
        "\n",
        "\n\n\n",
        "   ",
        "\u{2022}",
        "\u{2022} ",
        "\u{203A}",
        "\t\ttabbed",
        "- ",
        "1. ",
        "12345678901234567890. not a list",
        "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
        "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}",
        "a\u{0301}",
    ] {
        let _ = heuristic::rewrap(selection, true);
        let _ = heuristic::rewrap(selection, false);
    }
}

/// The dense form appears twice, but only the later occurrence has the backtick
/// sitting between its characters, so the returned slice says which one was
/// taken.
#[test]
fn repeated_text_resolves_to_the_last_occurrence() {
    let found = align::find(
        &haystack(&[
            "The first block has same text",
            "The later block has same `text`",
        ]),
        &DenseIndex::build("same text"),
        Options::default(),
    )
    .unwrap();
    assert_eq!(found.candidates, 2);
    // The closing backtick is past the last retained character, so it falls
    // outside the span; taking the earlier occurrence would give "same text".
    assert_eq!(found.text, "same `text");
}
