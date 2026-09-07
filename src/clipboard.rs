//! Reading and writing the system clipboard.
//!
//! Over SSH there is no local clipboard to talk to, so writes are emitted as an
//! OSC 52 escape sequence for the terminal emulator to pick up. Reads have no
//! such escape hatch — the sequence exists but is disabled by default in every
//! terminal worth using — so remote runs take their input from stdin instead.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};

/// Whether this process is talking to a terminal on another machine.
pub fn is_remote() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

pub fn read() -> Result<String> {
    if is_remote() {
        bail!(
            "cannot read the clipboard over SSH; pipe the selection in instead, \
             e.g. `pbpaste | ssh <host> termcopy --stdin --stdout`"
        );
    }
    arboard::Clipboard::new()
        .context("opening the clipboard")?
        .get_text()
        .context("reading the clipboard")
}

pub fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading stdin")?;
    Ok(text)
}

pub fn write(text: &str) -> Result<()> {
    if is_remote() {
        return write_osc52(text);
    }
    arboard::Clipboard::new()
        .context("opening the clipboard")?
        .set_text(text.to_string())
        .context("writing the clipboard")
    // On Linux a clipboard offer disappears when its owner exits, so this will
    // need `SetExtLinux::wait` and a lingering process. macOS and Windows hand
    // the text to the system, so there is nothing to hold on to.
}

/// Terminals cap how long an OSC 52 sequence they will accept and discard
/// anything longer without saying so. The usual ceiling is around 74KB of
/// encoded payload, and multiplexers set it lower, so this stays well under.
const OSC52_LIMIT: usize = 32 * 1024;

/// Hand the text to the terminal emulator over OSC 52.
///
/// Written to the terminal rather than stdout so that `--stdout` and shell
/// redirection stay usable at the same time.
fn write_osc52(text: &str) -> Result<()> {
    let encoded = base64(text.as_bytes());
    // Sending it anyway would be silently dropped by the terminal while this
    // process reported success, leaving the clipboard holding something else.
    if encoded.len() > OSC52_LIMIT {
        bail!(
            "{} characters is too long to send to the terminal's clipboard; \
             use --stdout and pipe it instead",
            text.chars().count()
        );
    }
    let mut terminal = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .context("opening /dev/tty to send the clipboard escape sequence")?;
    write!(terminal, "\x1b]52;c;{encoded}\x07").context("writing the clipboard escape sequence")?;
    terminal.flush().context("flushing /dev/tty")
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = chunk
            .iter()
            .enumerate()
            .fold(0u32, |packed, (offset, byte)| {
                packed | (u32::from(*byte) << (16 - 8 * offset))
            });
        for slot in 0..4 {
            if slot <= chunk.len() {
                out.push(ALPHABET[(packed >> (18 - 6 * slot) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_multibyte_text() {
        assert_eq!(base64("\u{4E00}".as_bytes()), "5LiA");
        assert_eq!(base64("go\u{1F4A1}".as_bytes()), "Z2/wn5Kh");
    }
}
