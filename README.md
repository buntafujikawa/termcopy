# termcopy

Restore a Codex TUI selection to the text it was rendered from.

For a one-off restoration, select text in a Codex pane, copy it, and run
`termcopy`. When a transcript contains a verified match, the clipboard receives
the corresponding source-text slice.

```console
$ termcopy
termcopy: restored 92 characters from session 11111111 (assistant message, exact match)
```

On macOS, start Codex through termcopy to enable automatic restoration for that
Codex process:

```console
$ termcopy run codex
```

The watcher exits when Codex exits.

## The problem

Copying from a terminal gives you text reconstructed from a rendered pane, not
necessarily the text that the author wrote. A narrow pane may show a message
like this:

```
• The schema is confirmed. The target is
  sample_items, and the member count
  comes from sample_members.
```

The source text may have been one line:

```
The schema is confirmed. The target is `sample_items`, and the member count comes from `sample_members`.
```

The terminal can add a gutter and continuation indentation, wrap long lines,
and render Markdown markers as styling.

## Why joining lines is ambiguous

Cleaning the copied text by joining lines cannot reliably distinguish a visual
line wrap from a newline written by the author. A wrapped English line often
consumes a space, while a long word may be split without a space. Scripts whose
words are not separated by spaces must not gain spaces when they wrap. Code,
logs, and list items may also contain intentional line breaks.

## How termcopy works

The program searches the Codex session transcript instead of guessing from the
rendered selection:

1. Read the selection from the clipboard.
2. Ask Herdr, when available, which pane is running which Codex session so that
   likely transcripts are searched first.
3. Compare a dense representation of the selection with the transcript while
   ignoring renderer whitespace and known presentation markers.
4. Return the matching span from the loaded transcript.
5. Write the result back to the clipboard.

When a verified match is found, intentional newlines, indentation, code, URLs,
and Unicode come from the stored transcript. A match is not guaranteed when the
selection contains UI-only text, encrypted thinking output, or too little
context around a rendered link.

## Install

Requires Rust 1.85 or later (2024 edition).

```console
$ cargo install --path .
```

## Usage

One-off mode:

```console
$ termcopy [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--session <REF>` | Search one session, given as a UUID or a path to a rollout file |
| `--pane <ID>` | Herdr pane the selection came from, for example `w1:p1` |
| `--stdin` | Read the selection from stdin instead of the clipboard |
| `--stdout` | Print the result instead of writing it back to the clipboard |
| `--fallback <MODE>` | What to do when nothing matches: `off`, `clean` (default), or `rewrap` |
| `--min-chars <N>` | Shortest selection worth looking up (default 4) |
| `--scan-limit <N>` | How many recent sessions to search (default 20) |
| `-v, --verbose` | Report where the match came from |

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Matched against a transcript; the result is verified against stored text |
| `2` | No match; the result was not verified against a transcript |
| `1` | Error |

### Automatic mode on macOS

Start Codex through the wrapper:

```console
$ termcopy run codex
```

Codex arguments are passed through unchanged:

```console
$ termcopy run codex --model gpt-5.6
```

While that process is alive, termcopy watches for clipboard text changes. It
attempts restoration only when the application that was frontmost when the
wrapper started is frontmost again. A copy made in another application is
marked as seen and is not processed later merely because the terminal becomes
frontmost.

If the launching application cannot be identified, termcopy falls back to the
configured default terminal applications. When Herdr provides a pane ID,
automatic mode also requires that pane to remain focused and to run Codex.
Without Herdr, recent sessions in the current directory are searched as in
one-off mode.

Automatic mode is verified-only. If the selection is not found in a Codex
transcript, the clipboard is left untouched; `clean` and `rewrap` are not
applied automatically.

The detected terminal application can be replaced with one or more
`--terminal-app` filters. Each filter is matched case-insensitively against the
frontmost application's name and bundle ID:

```console
$ termcopy run --terminal-app WezTerm codex
```

Other wrapper options:

| Option | Meaning |
| --- | --- |
| `--poll-ms <MS>` | Clipboard polling interval (default 50 ms, minimum 10 ms) |
| `--min-chars <N>` | Shortest selection worth looking up (default 4) |
| `--scan-limit <N>` | Recent sessions to search without a Herdr pane (default 20) |

### One-off use without leaving the keyboard

Bind one-off `termcopy` at the operating-system level, for example with a
script command, a desktop automation binding, or a shell alias in a neighboring
pane.

## What may not be found

Some rendered content is not present in a transcript, so termcopy reports no
verified match rather than fabricating source text. Examples include:

- UI chrome such as approval banners, timing rows, changed-file summaries, and
  transcript markers.
- Thinking output that the current Codex version stores in encrypted form.
- Context rows in a diff that contain only a line number.
- A short selection that is mostly a Markdown link and has too little surrounding
  prose to identify a unique transcript span.

## When there is no match

`--fallback` controls the unverified result, and stderr reports which mode was
used:

| Mode | Behaviour |
| --- | --- |
| `off` | Leave the selection unchanged |
| `clean` *(default)* | Remove renderer gutters and indentation while keeping line breaks |
| `rewrap` | Also join rows that appear to be visual wraps |

`clean` and `rewrap` operate without transcript verification. `rewrap` can join
code or log rows that were intended to remain separate, so use it only when
that trade-off is acceptable.

## Over SSH

Codex transcripts live on the machine where Codex runs, so termcopy must run
there as well. A remote process cannot read your local clipboard, so pipe the
selection in:

```console
$ pbpaste | ssh <host> termcopy --stdin --stdout
```

Without `--stdout`, termcopy writes the result over SSH through an OSC 52 escape
sequence that the terminal emulator can handle. With `--stdout`, it prints the
result only and does not write to the clipboard, which is useful for pipelines.

## Layout

| File | Responsibility |
| --- | --- |
| `src/codex.rs` | Find rollout files and read them into logical text blocks |
| `src/herdr.rs` | Ask Herdr which pane runs which session and rank candidates |
| `src/normalize.rs` | Build the dense view and map it back to original bytes |
| `src/align.rs` | Locate a selection in a transcript |
| `src/clipboard.rs` | Read and write local or OSC 52 clipboard data |
| `src/heuristic.rs` | Implement the unverified fallback |
| `src/automatic.rs` | Manage the scoped macOS watcher and wrapped command |
| `src/main.rs` | Parse arguments and run the one-off pipeline |

## Development

```console
$ cargo test
$ cargo clippy --all-targets --all-features -- -D warnings
$ cargo fmt --check
```

## Compatibility

The parser accepts the supported Codex rollout record shapes and skips records
that do not parse. A future format change may therefore produce fewer matches,
but malformed records are not treated as a process-wide failure.
