#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::{Result, bail};
use std::ffi::OsString;
use std::process::ExitCode;

#[cfg(target_os = "macos")]
use std::io::Write;
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::Duration;
#[cfg(target_os = "macos")]
use termcopy::herdr;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct RunOptions {
    pub terminal_apps: Vec<String>,
    pub poll_ms: u64,
    pub min_chars: usize,
    pub scan_limit: usize,
    pub command: Vec<OsString>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct WatchOptions {
    pub parent_pid: u32,
    pub pane: Option<String>,
    pub terminal_apps: Vec<String>,
    pub poll_ms: u64,
    pub min_chars: usize,
    pub scan_limit: usize,
}

pub fn run(options: RunOptions) -> Result<ExitCode> {
    #[cfg(target_os = "macos")]
    {
        run_macos(options)
    }

    #[cfg(not(target_os = "macos"))]
    {
        drop(options);
        bail!("`termcopy run` is currently supported only on macOS")
    }
}

pub fn watch(options: WatchOptions) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        watch_macos(options)
    }

    #[cfg(not(target_os = "macos"))]
    {
        drop(options);
        bail!("the automatic clipboard watcher is currently supported only on macOS")
    }
}

#[cfg(target_os = "macos")]
fn run_macos(options: RunOptions) -> Result<ExitCode> {
    use std::os::unix::process::CommandExt;

    let (program, command_args) = options
        .command
        .split_first()
        .context("a command is required after `termcopy run`")?;
    let executable = std::env::current_exe().context("locating the termcopy executable")?;
    let launched_from = if options.terminal_apps.is_empty() {
        frontmost_application().ok()
    } else {
        None
    };
    let terminal_apps = terminal_filters(&options.terminal_apps, launched_from.as_ref());

    let mut watcher = Command::new(executable);
    watcher
        .arg("__watch")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--poll-ms")
        .arg(options.poll_ms.to_string())
        .arg("--min-chars")
        .arg(options.min_chars.to_string())
        .arg("--scan-limit")
        .arg(options.scan_limit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    if let Some(pane_id) = herdr::current_pane_id() {
        watcher.arg("--pane").arg(pane_id);
    }
    for app in &terminal_apps {
        watcher.arg("--terminal-app").arg(app);
    }

    // Keep the watcher out of the wrapped command's foreground process group.
    // Ctrl-C should reach Codex without killing clipboard restoration early.
    watcher.process_group(0);
    let mut watcher_child = watcher.spawn().context("starting the clipboard watcher")?;

    // Preserve the wrapper PID so the watcher can use its parent PID as the
    // lifetime of the Codex process, while Herdr still sees an ordinary Codex.
    let error = Command::new(program).args(command_args).exec();
    let _ = watcher_child.kill();
    let _ = watcher_child.wait();
    Err(error).with_context(|| format!("launching {}", program.to_string_lossy()))
}

#[cfg(target_os = "macos")]
fn watch_macos(options: WatchOptions) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("opening the clipboard")?;
    let mut last_seen = clipboard.get_text().ok();
    let interval = Duration::from_millis(options.poll_ms.max(10));

    while parent_is(options.parent_pid) {
        std::thread::sleep(interval);
        if !parent_is(options.parent_pid) {
            break;
        }

        let selection = match clipboard.get_text() {
            Ok(text) => text,
            // Images and other non-text representations are outside termcopy's
            // scope and should remain untouched.
            Err(_) => continue,
        };
        if last_seen.as_deref() == Some(selection.as_str()) {
            continue;
        }

        // Record every text change before filtering it. A browser copy ignored
        // here must not be processed after the user returns to the terminal.
        last_seen = Some(selection.clone());

        let frontmost = match frontmost_application() {
            Ok(app) => app,
            // Fail closed when the source application cannot be identified.
            Err(_) => continue,
        };
        if !is_allowed_terminal(&frontmost, &options.terminal_apps) {
            continue;
        }
        if options
            .pane
            .as_deref()
            .is_some_and(|pane_id| !is_focused_codex_pane(pane_id))
        {
            continue;
        }

        let restored = match restore_selection(&selection, &options) {
            Ok(Some(text)) => text,
            Ok(None) | Err(_) => continue,
        };
        if restored == selection || !parent_is(options.parent_pid) {
            continue;
        }

        // Transcript matching can take long enough for another copy to happen.
        // Never replace a newer clipboard value with the result of an older one.
        if clipboard.get_text().ok().as_deref() != Some(selection.as_str()) {
            continue;
        }
        if clipboard.set_text(restored.clone()).is_ok() {
            last_seen = Some(restored);
        }
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn restore_selection(selection: &str, options: &WatchOptions) -> Result<Option<String>> {
    let executable = std::env::current_exe().context("locating the termcopy executable")?;
    let mut command = Command::new(executable);
    command
        .arg("--stdin")
        .arg("--stdout")
        .arg("--fallback")
        .arg("off")
        .arg("--min-chars")
        .arg(options.min_chars.to_string())
        .arg("--scan-limit")
        .arg(options.scan_limit.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(pane_id) = &options.pane {
        command.arg("--pane").arg(pane_id);
    }

    let mut child = command.spawn().context("starting termcopy restoration")?;
    child
        .stdin
        .take()
        .context("opening restoration stdin")?
        .write_all(selection.as_bytes())
        .context("sending the selection to termcopy")?;
    let output = child
        .wait_with_output()
        .context("waiting for termcopy restoration")?;
    if !output.status.success() {
        return Ok(None);
    }

    String::from_utf8(output.stdout)
        .map(Some)
        .context("restored text was not valid UTF-8")
}

#[cfg(target_os = "macos")]
fn is_focused_codex_pane(pane_id: &str) -> bool {
    herdr::panes()
        .iter()
        .find(|pane| pane.pane_id == pane_id)
        .is_some_and(|pane| pane.focused && pane.codex_session().is_some())
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn getppid() -> i32;
}

#[cfg(target_os = "macos")]
fn parent_is(expected: u32) -> bool {
    // SAFETY: getppid takes no arguments and has no memory-safety preconditions.
    unsafe { getppid() as u32 == expected }
}

#[cfg(any(target_os = "macos", test))]
const DEFAULT_TERMINAL_APPS: &[&str] = &["Ghostty", "cmux"];

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
struct FrontmostApplication {
    name: String,
    bundle_id: String,
}

#[cfg(target_os = "macos")]
fn frontmost_application() -> Result<FrontmostApplication> {
    const SCRIPT: &str = r#"
ObjC.import("AppKit");
var app = $.NSWorkspace.sharedWorkspace.frontmostApplication;
var name = ObjC.unwrap(app.localizedName) || "";
var bundle = ObjC.unwrap(app.bundleIdentifier) || "";
name + "\t" + bundle;
"#;

    let output = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", SCRIPT])
        .output()
        .context("querying the frontmost macOS application")?;
    if !output.status.success() {
        bail!(
            "could not query the frontmost macOS application: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout)
        .context("frontmost application name was not valid UTF-8")?;
    parse_frontmost_application(&stdout)
}

#[cfg(any(target_os = "macos", test))]
fn parse_frontmost_application(output: &str) -> Result<FrontmostApplication> {
    let mut fields = output.trim_end().splitn(2, '\t');
    let name = fields.next().unwrap_or_default().trim().to_string();
    let bundle_id = fields.next().unwrap_or_default().trim().to_string();
    if name.is_empty() && bundle_id.is_empty() {
        bail!("frontmost application was empty");
    }
    Ok(FrontmostApplication { name, bundle_id })
}

#[cfg(any(target_os = "macos", test))]
fn is_allowed_terminal(app: &FrontmostApplication, configured: &[String]) -> bool {
    if configured.is_empty() {
        DEFAULT_TERMINAL_APPS
            .iter()
            .any(|filter| app_matches_filter(app, filter))
    } else {
        configured
            .iter()
            .any(|filter| app_matches_filter(app, filter))
    }
}

#[cfg(any(target_os = "macos", test))]
fn app_matches_filter(app: &FrontmostApplication, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return false;
    }
    app.name.to_lowercase().contains(&filter) || app.bundle_id.to_lowercase().contains(&filter)
}

#[cfg(any(target_os = "macos", test))]
fn terminal_filters(
    configured: &[String],
    launched_from: Option<&FrontmostApplication>,
) -> Vec<String> {
    if !configured.is_empty() {
        return configured.to_vec();
    }

    let Some(app) = launched_from else {
        return Vec::new();
    };
    [app.name.clone(), app.bundle_id.clone()]
        .into_iter()
        .filter(|filter| !filter.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmost_application_name_and_bundle() {
        assert_eq!(
            parse_frontmost_application("Ghostty\tcom.mitchellh.ghostty\n").unwrap(),
            FrontmostApplication {
                name: "Ghostty".to_string(),
                bundle_id: "com.mitchellh.ghostty".to_string(),
            }
        );
    }

    #[test]
    fn terminal_filter_accepts_defaults_and_rejects_browser() {
        let ghostty = FrontmostApplication {
            name: "Ghostty".to_string(),
            bundle_id: "com.mitchellh.ghostty".to_string(),
        };
        let chrome = FrontmostApplication {
            name: "Google Chrome".to_string(),
            bundle_id: "com.google.Chrome".to_string(),
        };
        assert!(is_allowed_terminal(&ghostty, &[]));
        assert!(!is_allowed_terminal(&chrome, &[]));
    }

    #[test]
    fn custom_terminal_filter_can_match_bundle_id() {
        let app = FrontmostApplication {
            name: "My Terminal Preview".to_string(),
            bundle_id: "dev.example.custom-terminal".to_string(),
        };
        assert!(is_allowed_terminal(&app, &["custom-terminal".to_string()]));
    }

    #[test]
    fn empty_configuration_uses_the_application_that_launched_the_wrapper() {
        let app = FrontmostApplication {
            name: "My Terminal Preview".to_string(),
            bundle_id: "dev.example.custom-terminal".to_string(),
        };
        assert_eq!(
            terminal_filters(&[], Some(&app)),
            vec![
                "My Terminal Preview".to_string(),
                "dev.example.custom-terminal".to_string()
            ]
        );
    }

    #[test]
    fn explicit_terminal_filters_override_detected_application() {
        let app = FrontmostApplication {
            name: "My Terminal Preview".to_string(),
            bundle_id: "dev.example.custom-terminal".to_string(),
        };
        let configured = vec!["WezTerm".to_string()];
        assert_eq!(terminal_filters(&configured, Some(&app)), configured);
    }
}
