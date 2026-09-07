//! Restore a terminal selection copied out of the Codex TUI to the logical
//! source text it was rendered from.
//!
//! The TUI wraps text at the pane width, adds decorative gutters, and renders
//! Markdown markers as styling. A copied selection therefore cannot always be
//! repaired by inspecting it alone: English wraps may consume a space, while
//! scripts without inter-word spaces must not gain one. Instead of guessing,
//! termcopy locates the selection inside the session transcript and returns the
//! original slice.

pub mod align;
pub mod clipboard;
pub mod codex;
pub mod herdr;
pub mod heuristic;
pub mod normalize;
