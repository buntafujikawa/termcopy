use anyhow::{Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use termcopy::align::{self, Haystack, Match};
use termcopy::normalize::DenseIndex;
use termcopy::{clipboard, codex, herdr, heuristic};

mod automatic;

/// Restore a Codex TUI selection to the text it was rendered from.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,

    #[command(flatten)]
    restore: RestoreArgs,
}

#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Run Codex with automatic clipboard restoration enabled for its lifetime.
    Run(RunArgs),

    /// Internal clipboard watcher started by `termcopy run`.
    #[command(name = "__watch", hide = true)]
    Watch(WatchArgs),
}

#[derive(ClapArgs, Debug)]
struct RestoreArgs {
    /// Session to search, given as a UUID or a path to a rollout file.
    #[arg(long, value_name = "REF")]
    session: Option<String>,

    /// Herdr pane the selection came from, e.g. `w1:p1`.
    #[arg(long, value_name = "ID", conflicts_with = "session")]
    pane: Option<String>,

    /// Read the selection from stdin instead of the clipboard.
    #[arg(long)]
    stdin: bool,

    /// Print the result instead of writing it back to the clipboard.
    #[arg(long)]
    stdout: bool,

    /// Shortest selection worth looking up. Below this, matches are coincidence.
    #[arg(long, value_name = "N", default_value_t = 4)]
    min_chars: usize,

    /// How many recent sessions to search when none is given.
    #[arg(long, value_name = "N", default_value_t = 20)]
    scan_limit: usize,

    /// What to do when the selection is not in any transcript.
    #[arg(long, value_enum, default_value_t = Fallback::Clean)]
    fallback: Fallback,

    /// Report where the match came from.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(ClapArgs, Debug)]
#[command(trailing_var_arg = true)]
struct RunArgs {
    /// Frontmost macOS application allowed to trigger restoration. Repeatable.
    /// Defaults to Ghostty and cmux.
    #[arg(long = "terminal-app", value_name = "NAME")]
    terminal_apps: Vec<String>,

    /// Clipboard polling interval in milliseconds.
    #[arg(long, value_name = "MS", default_value_t = 50)]
    poll_ms: u64,

    /// Shortest selection worth looking up.
    #[arg(long, value_name = "N", default_value_t = 4)]
    min_chars: usize,

    /// How many recent sessions to search when Herdr does not identify the pane.
    #[arg(long, value_name = "N", default_value_t = 20)]
    scan_limit: usize,

    /// Command to run, for example `termcopy run codex`.
    #[arg(required = true, num_args = 1.., value_name = "COMMAND")]
    command: Vec<OsString>,
}

#[derive(ClapArgs, Debug)]
struct WatchArgs {
    #[arg(long)]
    parent_pid: u32,

    #[arg(long)]
    pane: Option<String>,

    #[arg(long = "terminal-app", value_name = "NAME")]
    terminal_apps: Vec<String>,

    #[arg(long, default_value_t = 50)]
    poll_ms: u64,

    #[arg(long, default_value_t = 4)]
    min_chars: usize,

    #[arg(long, default_value_t = 20)]
    scan_limit: usize,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Debug)]
enum Fallback {
    /// Leave the selection exactly as it was.
    Off,
    /// Remove gutters and the indentation the TUI added, keeping line breaks.
    Clean,
    /// Also run wrapped rows together, which un-wraps prose but damages code.
    Rewrap,
}

fn main() -> ExitCode {
    match dispatch() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("termcopy: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(CliCommand::Run(args)) => automatic::run(automatic::RunOptions {
            terminal_apps: args.terminal_apps,
            poll_ms: args.poll_ms,
            min_chars: args.min_chars,
            scan_limit: args.scan_limit,
            command: args.command,
        }),
        Some(CliCommand::Watch(args)) => {
            automatic::watch(automatic::WatchOptions {
                parent_pid: args.parent_pid,
                pane: args.pane,
                terminal_apps: args.terminal_apps,
                poll_ms: args.poll_ms,
                min_chars: args.min_chars,
                scan_limit: args.scan_limit,
            })?;
            Ok(ExitCode::SUCCESS)
        }
        None => run(&cli.restore).map(Outcome::exit_code),
    }
}

enum Outcome {
    /// The selection was found in a transcript and replaced with the original.
    Restored,
    /// The command worked, but the result was never checked against a
    /// transcript. Given its own code so scripts can tell the two apart.
    Unverified,
}

impl Outcome {
    fn exit_code(self) -> ExitCode {
        match self {
            Outcome::Restored => ExitCode::SUCCESS,
            Outcome::Unverified => ExitCode::from(2),
        }
    }
}

fn run(args: &RestoreArgs) -> Result<Outcome> {
    let selection = if args.stdin {
        clipboard::read_stdin()?
    } else {
        clipboard::read()?
    };

    let dense = DenseIndex::from_rendered(&selection);
    // Too little text to identify: at this length a match would be a
    // coincidence, and a coincidence would be worse than doing nothing.
    if dense.len() < args.min_chars {
        eprintln!(
            "termcopy: selection is too short to look up ({} characters, minimum {})",
            dense.len(),
            args.min_chars
        );
        return unverified(args, &selection);
    }

    let home = codex::codex_home()?;
    let candidates = candidate_sessions(args, &home)?;
    if candidates.is_empty() {
        eprintln!("termcopy: no Codex sessions found under {}", home.display());
        return unverified(args, &selection);
    }

    // `--min-chars` guards both gates: whether a selection is worth looking up
    // at all, and how far a match may be pared back before it means nothing.
    let options = align::Options {
        min_span: args.min_chars,
        ..align::Options::default()
    };
    match search(&candidates, &dense, options) {
        Some((transcript, found)) => {
            report(&transcript, &found);
            emit(args, &found.text)?;
            Ok(Outcome::Restored)
        }
        None => {
            eprintln!(
                "termcopy: could not find this selection in {} session{}",
                candidates.len(),
                if candidates.len() == 1 { "" } else { "s" }
            );
            unverified(args, &selection)
        }
    }
}

/// Every path that could not check the text against a transcript ends here, so
/// `--fallback` means the same thing whichever way the search came up empty.
fn unverified(args: &RestoreArgs, selection: &str) -> Result<Outcome> {
    let text = match args.fallback {
        Fallback::Off => {
            eprintln!("termcopy: leaving it unchanged");
            selection.to_string()
        }
        Fallback::Clean => {
            eprintln!("termcopy: removed TUI decoration, but line breaks are as rendered");
            heuristic::rewrap(selection, false)
        }
        Fallback::Rewrap => {
            eprintln!("termcopy: guessed which line breaks were wrapping; check the result");
            heuristic::rewrap(selection, true)
        }
    };
    emit(args, &text)?;
    Ok(Outcome::Unverified)
}

/// Sessions to search, most promising first.
///
/// An explicit `--session` or `--pane` settles it. Otherwise Herdr is asked
/// which panes are running Codex, ranked by how close they are to wherever
/// termcopy was started, and recent sessions are appended so the search still
/// works when Herdr has nothing to say.
fn candidate_sessions(args: &RestoreArgs, home: &std::path::Path) -> Result<Vec<PathBuf>> {
    if let Some(reference) = &args.session {
        return Ok(vec![codex::resolve_session_ref(home, reference)?]);
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    let mut push = |path: PathBuf| {
        if !paths.contains(&path) {
            paths.push(path);
        }
    };

    if let Some(pane_id) = &args.pane {
        let session = herdr::session_of(&herdr::panes(), pane_id)
            .with_context(|| format!("pane {pane_id} is not running a Codex session"))?;
        return Ok(vec![codex::resolve_session_ref(home, &session)?]);
    }

    let panes = herdr::panes();
    for session in herdr::ranked_session_ids(&panes, herdr::current_pane_id().as_deref()) {
        if let Some(path) = codex::find_by_id(home, &session) {
            push(path);
        }
    }
    for path in recent_by_locality(home, args.scan_limit) {
        push(path);
    }

    Ok(paths)
}

/// Recent sessions, with the ones started in the current directory first. Panes
/// running elsewhere are usually working on something else entirely.
fn recent_by_locality(home: &std::path::Path, limit: usize) -> Vec<PathBuf> {
    let mut recent = codex::recent(home, limit);
    let Ok(cwd) = std::env::current_dir() else {
        return recent;
    };
    let cwd = cwd.to_string_lossy().into_owned();
    // Cached, because the key opens and reads a file and a plain sort would ask
    // for it O(n log n) times.
    recent.sort_by_cached_key(|path| codex::session_cwd(path).as_deref() != Some(cwd.as_str()));
    recent
}

/// Take the first session the selection can be found in. Sessions arrive newest
/// first, so the most recent transcript that can account for the text wins.
fn search(
    candidates: &[PathBuf],
    selection: &DenseIndex,
    options: align::Options,
) -> Option<(codex::Transcript, Match)> {
    for path in candidates {
        let transcript = match codex::load(path) {
            Ok(transcript) => transcript,
            Err(error) => {
                // Always reported: a session that cannot be read is the reason
                // for an empty result, and hiding it behind --verbose makes an
                // unreadable `--session` argument look like a failed search.
                eprintln!("termcopy: skipping {}: {error:#}", path.display());
                continue;
            }
        };
        let haystack = Haystack::build(&transcript.blocks);
        if let Some(found) = align::find(&haystack, selection, options) {
            return Some((transcript, found));
        }
    }
    None
}

fn report(transcript: &codex::Transcript, found: &Match) {
    let mut detail = found.strategy.label().to_string();
    if let Some(kind) = found.block {
        detail = format!("{}, {detail}", kind.label());
    }
    if found.candidates > 1 {
        detail.push_str(&format!(
            ", most recent of {} occurrences",
            found.candidates
        ));
    }
    eprintln!(
        "termcopy: restored {} characters from session {} ({detail})",
        found.text.chars().count(),
        transcript.short_id()
    );
}

fn emit(args: &RestoreArgs, text: &str) -> Result<()> {
    if args.stdout {
        // `print!` panics when the pipe closes, which is ordinary in a pipeline
        // like `termcopy --stdout | head`. Treat it as the reader being done.
        return match std::io::stdout().write_all(text.as_bytes()) {
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            other => other.context("writing the result to stdout"),
        };
    }
    clipboard::write(text).context("writing the result to the clipboard")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_invocation_still_selects_one_shot_mode() {
        let cli =
            Cli::try_parse_from(["termcopy", "--fallback", "off", "--scan-limit", "3"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.restore.fallback, Fallback::Off);
        assert_eq!(cli.restore.scan_limit, 3);
    }

    #[test]
    fn run_requires_command() {
        assert!(Cli::try_parse_from(["termcopy", "run"]).is_err());
    }

    #[test]
    fn run_preserves_codex_arguments() {
        let cli = Cli::try_parse_from([
            "termcopy",
            "run",
            "--poll-ms",
            "25",
            "codex",
            "--model",
            "gpt-5.6",
        ])
        .unwrap();
        let Some(CliCommand::Run(args)) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.poll_ms, 25);
        assert_eq!(
            args.command,
            ["codex", "--model", "gpt-5.6"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }
}
