//! Locating and reading Codex rollout transcripts.
//!
//! Sessions live at `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local-iso>-<uuid>.jsonl`
//! with older ones moved to `$CODEX_HOME/archived_sessions/`. Each line is a
//! JSON record; the ones carrying rendered text are `event_msg`/`item_completed`
//! (Codex >= 0.147) and `response_item` (all versions), so both are read and
//! de-duplicated by text.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Where a block of transcript text came from. Diagnostics only — matching
/// treats every block the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    AgentMessage,
    UserMessage,
    Reasoning,
    Command,
    CommandOutput,
}

impl BlockKind {
    pub fn label(self) -> &'static str {
        match self {
            BlockKind::AgentMessage => "assistant message",
            BlockKind::UserMessage => "user message",
            BlockKind::Reasoning => "reasoning",
            BlockKind::Command => "command",
            BlockKind::CommandOutput => "command output",
        }
    }
}

/// One logical chunk of transcript text, in the order Codex emitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub cli_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Transcript {
    pub path: PathBuf,
    pub meta: SessionMeta,
    pub blocks: Vec<Block>,
}

impl Transcript {
    /// Short session identifier for status output, falling back to the file stem.
    pub fn short_id(&self) -> String {
        match &self.meta.id {
            Some(id) => id.split('-').next().unwrap_or(id).to_string(),
            None => self
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// `$CODEX_HOME`, or `~/.codex` when unset.
pub fn codex_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home));
    }
    let home =
        std::env::var_os("HOME").ok_or_else(|| anyhow!("neither CODEX_HOME nor HOME is set"))?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Resolve a `--session` argument, which is either a path to a rollout file or a
/// session UUID.
pub fn resolve_session_ref(home: &Path, reference: &str) -> Result<PathBuf> {
    let as_path = Path::new(reference);
    if as_path.is_file() {
        return Ok(as_path.to_path_buf());
    }
    find_by_id(home, reference).ok_or_else(|| {
        anyhow!(
            "no rollout file found for session `{reference}` under {}",
            home.display()
        )
    })
}

/// Find the rollout file whose name ends with `-<id>.jsonl`.
pub fn find_by_id(home: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("-{id}.jsonl");
    session_dirs(home)
        .into_iter()
        .flat_map(|dir| rollout_files(&dir))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&suffix))
        })
}

/// The `limit` most recently started sessions, newest first.
///
/// Rollout file names embed a zero-padded local timestamp, so sorting the
/// directory and file names in reverse lexical order is the same as sorting by
/// start time and avoids stat-ing several thousand files.
pub fn recent(home: &Path, limit: usize) -> Vec<PathBuf> {
    // `limit` comes straight from the command line, so it cannot be trusted as
    // an allocation size.
    let mut found = Vec::with_capacity(limit.min(1024));
    for dir in session_dirs(home) {
        for path in rollout_files(&dir) {
            found.push(path);
            if found.len() >= limit {
                return found;
            }
        }
    }
    found
}

/// The working directory a session was started in, read without parsing the
/// rest of the file. `session_meta` is always the first record, so this costs
/// one line rather than the whole transcript.
pub fn session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    let record: Value = serde_json::from_str(&first).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    string_field(record.get("payload")?, "cwd")
}

/// Read a rollout file into its logical text blocks.
pub fn load(path: &Path) -> Result<Transcript> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let (meta, blocks) = parse(BufReader::new(file));
    // A file with no session header and nothing to index is not a transcript.
    // Saying so beats reporting that the selection could not be found in it.
    if meta == SessionMeta::default() && blocks.is_empty() {
        bail!("{} is not a Codex rollout file", path.display());
    }
    Ok(Transcript {
        path: path.to_path_buf(),
        meta,
        blocks,
    })
}

fn parse<R: BufRead>(reader: R) -> (SessionMeta, Vec<Block>) {
    let mut meta = SessionMeta::default();
    let mut blocks: Vec<Block> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for line in reader.lines() {
        // A session being written to concurrently can end in a partial line, and
        // records from future Codex versions may not parse. Skip, never fail.
        let Ok(line) = line else { continue };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(payload) = record.get("payload") else {
            continue;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("session_meta") => meta = parse_session_meta(payload),
            Some("event_msg") => collect_event_msg(payload, &mut blocks, &mut seen),
            Some("response_item") => collect_response_item(payload, &mut blocks, &mut seen),
            _ => {}
        }
    }

    (meta, blocks)
}

/// Every `sessions/YYYY/MM/DD` directory plus `archived_sessions`, newest first.
fn session_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for year in sorted_dirs_desc(&home.join("sessions")) {
        for month in sorted_dirs_desc(&year) {
            dirs.extend(sorted_dirs_desc(&month));
        }
    }
    let archived = home.join("archived_sessions");
    if archived.is_dir() {
        dirs.push(archived);
    }
    dirs
}

fn sorted_dirs_desc(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort_unstable();
    dirs.reverse();
    dirs
}

fn rollout_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .collect();
    files.sort_unstable();
    files.reverse();
    files
}

fn parse_session_meta(payload: &Value) -> SessionMeta {
    SessionMeta {
        id: string_field(payload, "id"),
        cwd: string_field(payload, "cwd"),
        cli_version: string_field(payload, "cli_version"),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `event_msg`/`item_completed` mirrors what the TUI drew, one record per
/// rendered item.
fn collect_event_msg(payload: &Value, blocks: &mut Vec<Block>, seen: &mut HashSet<String>) {
    if payload.get("type").and_then(Value::as_str) != Some("item_completed") {
        return;
    }
    let Some(item) = payload.get("item") else {
        return;
    };
    match item.get("type").and_then(Value::as_str) {
        Some("AgentMessage") => push(
            blocks,
            seen,
            BlockKind::AgentMessage,
            join_text_parts(item.get("content")),
        ),
        Some("UserMessage") => push(
            blocks,
            seen,
            BlockKind::UserMessage,
            join_text_parts(item.get("content")),
        ),
        Some("Reasoning") => push(
            blocks,
            seen,
            BlockKind::Reasoning,
            join_text_parts(item.get("summary_text")),
        ),
        Some("CommandExecution") => {
            push(
                blocks,
                seen,
                BlockKind::Command,
                shell_script(item.get("command")),
            );
            push(
                blocks,
                seen,
                BlockKind::CommandOutput,
                string_field(item, "aggregated_output"),
            );
        }
        _ => {}
    }
}

/// `response_item` is the raw model conversation. It is the only source on Codex
/// < 0.147, but it also carries synthetic turns the TUI never showed.
fn collect_response_item(payload: &Value, blocks: &mut Vec<Block>, seen: &mut HashSet<String>) {
    match payload.get("type").and_then(Value::as_str) {
        Some("message") => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let text = join_text_parts(payload.get("content"));
            match role {
                "assistant" => push(blocks, seen, BlockKind::AgentMessage, text),
                "user" => {
                    let text = text.filter(|text| !is_synthetic_user_text(text));
                    push(blocks, seen, BlockKind::UserMessage, text);
                }
                // `developer` turns are injected instructions, never rendered.
                _ => {}
            }
        }
        Some("reasoning") => push(
            blocks,
            seen,
            BlockKind::Reasoning,
            join_text_parts(payload.get("summary")),
        ),
        // A sub-agent's reply, wrapped in a routing envelope. Matching finds the
        // prose inside it and returns only that, so the envelope never leaks
        // into the result.
        Some("agent_message") => push(
            blocks,
            seen,
            BlockKind::AgentMessage,
            join_text_parts(payload.get("content")),
        ),
        // The script Codex ran. On Codex < 0.147 this is the only record of the
        // command, and it also carries plan updates and patches.
        Some("custom_tool_call") => push(
            blocks,
            seen,
            BlockKind::Command,
            payload
                .get("input")
                .and_then(Value::as_str)
                .and_then(script_literals),
        ),
        Some("custom_tool_call_output") => {
            push(
                blocks,
                seen,
                BlockKind::CommandOutput,
                parse_tool_output(payload.get("output")),
            );
        }
        _ => {}
    }
}

/// Concatenate the `text` fields of a content array. Also accepts an array of
/// bare strings, which is how some item variants store their parts.
fn join_text_parts(value: Option<&Value>) -> Option<String> {
    let parts = value?.as_array()?;
    let mut joined = String::new();
    for part in parts {
        match part {
            Value::String(text) => joined.push_str(text),
            _ => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    joined.push_str(text);
                }
            }
        }
    }
    Some(joined)
}

/// `custom_tool_call_output.output` holds the command's output in one of three
/// shapes: an array of content parts, a JSON string holding such an array, or a
/// plain string. Roughly nine in ten records use the array form, so mishandling
/// it costs most of the command output in the transcript.
fn parse_tool_output(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if value.is_array() {
        return join_text_parts(Some(value));
    }
    let raw = value.as_str()?;
    match serde_json::from_str::<Value>(raw) {
        Ok(parsed) => join_text_parts(Some(&parsed)).or_else(|| Some(raw.to_string())),
        Err(_) => Some(raw.to_string()),
    }
}

/// The contents of the string literals in a tool call's JavaScript snippet.
///
/// A call arrives as source rather than data:
///
/// ```text
/// const patch = "*** Begin Patch\n+#!/bin/sh\n+set -eu\n"
/// const r = await tools.exec_command({cmd:"ls -la", workdir:"/tmp"})
/// ```
///
/// What the TUI drew is the decoded contents of those literals — real newlines,
/// real quotes — and never the scaffolding around them. Indexing the raw source
/// would therefore fail to match the command as displayed while filling the
/// haystack with text that was never on screen.
fn script_literals(input: &str) -> Option<String> {
    let mut literals: Vec<String> = Vec::new();
    let mut chars = input.chars();

    while let Some(ch) = chars.next() {
        if !matches!(ch, '"' | '\'' | '`') {
            continue;
        }
        let quote = ch;
        let mut literal = String::new();
        loop {
            match chars.next() {
                None => break,
                Some(end) if end == quote => break,
                Some('\\') => match chars.next() {
                    None => break,
                    Some('n') => literal.push('\n'),
                    Some('t') => literal.push('\t'),
                    Some('r') => literal.push('\r'),
                    Some('0') => literal.push('\0'),
                    // Anything else stands for itself, which covers \" \' \` \\
                    // and leaves unknown escapes readable.
                    Some(other) => literal.push(other),
                },
                Some(other) => literal.push(other),
            }
        }
        if !literal.trim().is_empty() {
            literals.push(literal);
        }
    }

    (!literals.is_empty()).then(|| literals.join("\n"))
}

/// Turn `["/bin/zsh", "-lc", "ls -la"]` into `ls -la`, which is what the TUI
/// displays. Anything that is not a shell wrapper is joined as-is.
fn shell_script(value: Option<&Value>) -> Option<String> {
    let parts: Vec<&str> = value?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if parts.len() >= 3 && matches!(parts[1], "-lc" | "-c" | "-lic") {
        return Some(parts[2..].join(" "));
    }
    Some(parts.join(" "))
}

/// Instruction blobs Codex injects as user turns. They are never rendered, so
/// matching against them could only ever produce a wrong answer.
fn is_synthetic_user_text(text: &str) -> bool {
    let text = text.trim_start();
    if text.starts_with("# AGENTS.md instructions for ") {
        return true;
    }
    // Named rather than "anything that opens with a tag", so a question like
    // "<div> should render how?" stays in the index and can still be restored.
    const INJECTED: [&str; 8] = [
        "<environment_context>",
        "<user_instructions>",
        "<skill>",
        "<skills_instructions>",
        "<turn_aborted>",
        "<recommended_plugins>",
        "<app-context>",
        "<collaboration_mode>",
    ];
    INJECTED.iter().any(|tag| text.starts_with(tag))
}

fn push(
    blocks: &mut Vec<Block>,
    seen: &mut HashSet<String>,
    kind: BlockKind,
    text: Option<String>,
) {
    let Some(text) = text else { return };
    if text.trim().is_empty() || !seen.insert(text.clone()) {
        return;
    }
    blocks.push(Block { kind, text });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks_from(lines: &[&str]) -> Vec<Block> {
        parse(lines.join("\n").as_bytes()).1
    }

    #[test]
    fn reads_agent_and_user_messages_from_item_completed() {
        let blocks = blocks_from(&[
            r#"{"type":"session_meta","payload":{"id":"session-current","cwd":"/fixture/worktree","cli_version":"0.147.0"}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Please write a query"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"The target is `sample_groups`."}]}}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![
                Block {
                    kind: BlockKind::UserMessage,
                    text: "Please write a query".into()
                },
                Block {
                    kind: BlockKind::AgentMessage,
                    text: "The target is `sample_groups`.".into()
                },
            ]
        );
    }

    #[test]
    fn reads_messages_from_response_item_on_older_sessions() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::AgentMessage,
                text: "done".into()
            }]
        );
    }

    #[test]
    fn splits_command_execution_into_script_and_output() {
        let blocks = blocks_from(&[
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","ls -la"],"aggregated_output":"AGENTS.md\nsrc\n"}}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![
                Block {
                    kind: BlockKind::Command,
                    text: "ls -la".into()
                },
                Block {
                    kind: BlockKind::CommandOutput,
                    text: "AGENTS.md\nsrc\n".into()
                },
            ]
        );
    }

    #[test]
    fn unwraps_double_encoded_tool_output() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":"[{\"type\":\"input_text\",\"text\":\"Output:\\n\"},{\"type\":\"input_text\",\"text\":\"README.md\\n\"}]"}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::CommandOutput,
                text: "Output:\nREADME.md\n".into()
            }]
        );
    }

    #[test]
    fn drops_developer_turns_and_injected_user_blobs() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<app-context>hi</app-context>"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<current_date>2026-08-08</current_date>\n</environment_context>"}]}}"#,
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /fixture/worktree\n\nrules"}]}}"##,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Please update the example-workflow"}]}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::UserMessage,
                text: "Please update the example-workflow".into()
            }]
        );
    }

    #[test]
    fn keeps_user_text_that_merely_starts_with_a_bracket() {
        assert!(!is_synthetic_user_text("[example-user]$ tool install"));
        assert!(!is_synthetic_user_text("<- this arrow is not a tag"));
        assert!(is_synthetic_user_text(
            "<skill>\n<name>example-workflow</name>"
        ));
    }

    /// Only the tags Codex actually injects are dropped. A question that opens
    /// with a tag is a real question and has to stay findable.
    #[test]
    fn keeps_a_question_that_opens_with_an_html_tag() {
        assert!(!is_synthetic_user_text("<div> should render how?"));
        assert!(!is_synthetic_user_text("<Button /> does not render"));
    }

    #[test]
    fn reads_the_array_form_of_tool_output() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"Output:\n"},{"type":"input_text","text":"README.md\n"}]}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::CommandOutput,
                text: "Output:\nREADME.md\n".into()
            }]
        );
    }

    #[test]
    fn reads_a_sub_agent_message() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"agent_message","content":[{"type":"input_text","text":"Payload:\nThe requested changes are listed here."}]}}"#,
        ]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::AgentMessage);
    }

    /// Only the contents of the string literals are indexed. The JavaScript
    /// around them was never drawn, so matching it is impossible and keeping it
    /// would just pad the haystack.
    #[test]
    fn reads_the_script_a_tool_call_ran() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"await tools.exec_command({cmd:\"ls -la\", workdir:\"/tmp\"})"}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::Command,
                text: "ls -la\n/tmp".into()
            }]
        );
    }

    /// A patch arrives as one long literal whose newlines and quotes are still
    /// escaped. The TUI drew the decoded form, so that is what has to be indexed.
    #[test]
    fn decodes_the_escapes_in_a_patch_literal() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"const patch = \"*** Begin Patch\\n+if [ \\\"$mode\\\" ]; then\\n+    printf '%s\\\\n' ok\\n\""}}"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::Command,
                // A doubled backslash in the source is one literal backslash, so
                // `printf '%s\n'` keeps its escape instead of becoming a newline.
                text: "*** Begin Patch\n+if [ \"$mode\" ]; then\n+    printf '%s\\n' ok\n".into()
            }]
        );
    }

    #[test]
    fn a_tool_call_with_no_string_literals_contributes_nothing() {
        let blocks = blocks_from(&[
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"await tools.noop()"}}"#,
        ]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_rollout_is_an_error_rather_than_an_empty_transcript() {
        let path = std::env::temp_dir().join("termcopy-not-a-rollout.txt");
        fs::write(&path, "127.0.0.1 localhost\n").unwrap();
        let error = load(&path).unwrap_err().to_string();
        fs::remove_file(&path).ok();
        assert!(error.contains("not a Codex rollout file"), "{error}");
    }

    #[test]
    fn deduplicates_text_repeated_across_both_record_kinds() {
        let blocks = blocks_from(&[
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"same text"}]}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"same text"}]}}"#,
        ]);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn skips_malformed_lines_instead_of_failing() {
        let blocks = blocks_from(&[
            "not json at all",
            "",
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"survived"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_c"#,
        ]);
        assert_eq!(
            blocks,
            vec![Block {
                kind: BlockKind::AgentMessage,
                text: "survived".into()
            }]
        );
    }
}
