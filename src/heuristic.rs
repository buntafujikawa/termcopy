//! Tidying a selection that could not be found in any transcript.
//!
//! This is the only guesswork in the program, and it is reached only once the
//! transcript has been ruled out. The caller says so on stderr and exits
//! non-zero.
//!
//! Two things happen here, and they are worth telling apart:
//!
//! * Removing gutters and the indentation the TUI added is unambiguous. The
//!   renderer put those columns there and nothing is lost by taking them away.
//! * Joining a row with the one below it is a guess. Whether a line ending was
//!   the renderer running out of width or something the author typed cannot be
//!   told from the rendered text — that is the whole reason this program reads
//!   transcripts. Prose wants the join; code and log output are damaged by it.
//!
//! So joining is a caller's decision, and structural cleanup always happens.

/// A rendered row with its decoration removed.
struct Row<'a> {
    /// Display columns before the content, gutter glyphs included.
    indent: usize,
    text: &'a str,
}

/// Strip TUI decoration from a selection.
///
/// With `join_wrapped`, rows that look like a continuation of the row above are
/// run together, which un-wraps prose at the cost of mangling anything whose
/// line breaks were meaningful.
pub fn rewrap(selection: &str, join_wrapped: bool) -> String {
    let selection = crate::normalize::strip_diff_line_numbers(selection);
    let rows: Vec<Row> = selection.lines().map(strip_gutter).collect();
    // The shallowest row sets the left edge. Subtracting it removes the indent
    // the TUI added while keeping indentation that belongs to the content.
    let base = rows
        .iter()
        .filter(|row| !row.text.is_empty())
        .map(|row| row.indent)
        .min()
        .unwrap_or(0);

    let mut out = String::new();
    let mut previous: Option<(usize, &str)> = None;
    let mut blank_seen = false;

    for row in &rows {
        if row.text.is_empty() {
            blank_seen = previous.is_some();
            continue;
        }
        let indent = row.indent.saturating_sub(base);

        match previous {
            None => {}
            Some(_) if blank_seen => out.push_str("\n\n"),
            Some((previous_indent, previous_text)) => {
                // A list marker starts something new, and text that steps to
                // the right is structure rather than an overflowing line.
                let continues =
                    join_wrapped && !starts_list_item(row.text) && indent <= previous_indent;
                match continues {
                    true if needs_space(previous_text, row.text) => out.push(' '),
                    true => {}
                    false => out.push('\n'),
                }
            }
        }
        if out.is_empty() || out.ends_with('\n') {
            out.extend(std::iter::repeat_n(' ', indent));
        }
        out.push_str(row.text);

        previous = Some((indent, row.text));
        blank_seen = false;
    }
    out
}

/// Strip leading whitespace and any gutter glyph, returning the columns removed
/// and the content. Both `"• text"` and `"  text"` land on the same column, so
/// a wrapped row lines up with the row it continues.
fn strip_gutter(line: &str) -> Row<'_> {
    let mut indent = 0;
    let mut rest = line;
    loop {
        let mut chars = rest.chars();
        match chars.next() {
            Some(ch) if ch.is_whitespace() || is_gutter_glyph(ch) => {
                indent += 1;
                rest = chars.as_str();
            }
            _ => break,
        }
    }
    Row {
        indent,
        text: rest.trim_end(),
    }
}

fn is_gutter_glyph(ch: char) -> bool {
    matches!(ch, '•' | '·' | '›' | '‹' | '▌' | '⏺' | '↳' | '⎿')
        || matches!(ch, '\u{2500}'..='\u{259F}')
}

fn starts_list_item(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix(['-', '*', '+']) {
        return rest.starts_with(' ');
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    matches!(text[digits..].strip_prefix(['.', ')']), Some(after) if after.starts_with(' '))
}

/// Whether joining two rows should put a space between them.
///
/// A wrap between Latin words consumes the space that was there; Japanese wraps
/// consume nothing. Going by the characters either side of the break gets this
/// right except for a single Latin word too long for the pane, which is
/// re-joined with a space that does not belong. The transcript settles that
/// case, which is why this function is only ever the fallback.
fn needs_space(previous: &str, next: &str) -> bool {
    let before = previous.chars().next_back();
    let after = next.chars().next();
    matches!((before, after), (Some(before), Some(after)) if before.is_ascii() && after.is_ascii())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(selection: &str) -> String {
        rewrap(selection, true)
    }

    fn cleaned(selection: &str) -> String {
        rewrap(selection, false)
    }

    #[test]
    fn joins_a_wrapped_latin_line_with_a_space() {
        let rendered =
            "• The deployment completed successfully, and the service is now\n  running normally.";
        assert_eq!(
            joined(rendered),
            "The deployment completed successfully, and the service is now running normally."
        );
    }

    #[test]
    fn joins_a_non_space_line_without_a_space() {
        let rendered = "• \u{4E00}\u{4E8C}\u{4E09}\u{56DB}\n  \u{4E94}\u{516D}\u{4E03}\u{516B}\n  \u{4E5D}\u{5341}";
        assert_eq!(
            joined(rendered),
            "\u{4E00}\u{4E8C}\u{4E09}\u{56DB}\u{4E94}\u{516D}\u{4E03}\u{516B}\u{4E5D}\u{5341}"
        );
    }

    #[test]
    fn removes_gutters_and_indentation_without_joining() {
        let rendered = "• Keep the transcript source\n  exactly as stored";
        assert_eq!(
            cleaned(rendered),
            "Keep the transcript source\nexactly as stored"
        );
    }

    #[test]
    fn keeps_a_blank_line_as_a_paragraph_break() {
        assert_eq!(
            joined("• First paragraph.\n\n  Second paragraph."),
            "First paragraph.\n\nSecond paragraph."
        );
    }

    #[test]
    fn keeps_each_list_item_on_its_own_line() {
        let rendered = "  - authenticated owners only\n  - audit logging is required\n  - plain output is forbidden";
        assert_eq!(
            joined(rendered),
            "- authenticated owners only\n- audit logging is required\n- plain output is forbidden"
        );
    }

    #[test]
    fn keeps_numbered_list_items_apart() {
        assert_eq!(
            joined("  1. Confirm the input\n  2. Update the result"),
            "1. Confirm the input\n2. Update the result"
        );
    }

    #[test]
    fn keeps_a_wrapped_list_item_that_is_indented_further_on_its_own_line() {
        let rendered = "  - group_id and status =\n    'active' limit the target";
        assert_eq!(
            joined(rendered),
            "- group_id and status =\n  'active' limit the target"
        );
    }

    /// Indentation is preserved either way, but rows at the same depth are run
    /// together when joining is on. Line breaks that carry meaning are exactly
    /// what cannot be recovered without the transcript, so code is the case to
    /// leave joining off for.
    #[test]
    fn preserves_code_only_when_joining_is_off() {
        let rendered = "  const config = {\n      retries: 3,\n      timeout: 5000,\n  };";
        assert_eq!(
            cleaned(rendered),
            "const config = {\n    retries: 3,\n    timeout: 5000,\n};"
        );
        assert_eq!(
            joined(rendered),
            "const config = {\n    retries: 3, timeout: 5000, };",
            "documents the damage joining does to code"
        );
    }

    #[test]
    fn strips_the_user_prompt_gutter_too() {
        assert_eq!(
            joined("› Change the project rule\n  now."),
            "Change the project rule now."
        );
    }

    #[test]
    fn leaves_a_single_line_untouched_apart_from_its_gutter() {
        assert_eq!(
            joined("• Deployment is complete."),
            "Deployment is complete."
        );
        assert_eq!(cleaned("plain text"), "plain text");
    }

    #[test]
    fn handles_empty_and_blank_only_input() {
        assert_eq!(joined(""), "");
        assert_eq!(joined("\n\n  \n"), "");
    }

    #[test]
    fn collapses_repeated_blank_rows_into_one_break() {
        assert_eq!(
            joined("• First half.\n\n\n\n  Second half."),
            "First half.\n\nSecond half."
        );
    }
}
