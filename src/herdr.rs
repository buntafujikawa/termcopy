//! Asking Herdr which Codex session the selection probably came from.
//!
//! Herdr tracks the agent running in each pane and records its session id, so
//! the pane a selection was dragged out of names its own transcript. That is
//! only ever a hint: the answer still has to be found in the transcript, so a
//! wrong guess here costs a few milliseconds rather than a wrong result.
//!
//! Everything degrades quietly. Herdr may not be installed, its socket may be
//! down, or termcopy may be running outside a pane entirely.

use serde::Deserialize;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Pane {
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    #[serde(default)]
    pub value: Option<String>,
}

impl Pane {
    /// The Codex session this pane is running, if it is running one.
    pub fn codex_session(&self) -> Option<&str> {
        if self.agent.as_deref() != Some("codex") {
            return None;
        }
        self.agent_session.as_ref()?.value.as_deref()
    }
}

#[derive(Deserialize)]
struct PaneListResponse {
    result: PaneList,
}

#[derive(Deserialize)]
struct PaneList {
    #[serde(default)]
    panes: Vec<Pane>,
}

/// The pane termcopy is running in, as told by the environment Herdr exports.
pub fn current_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|id| !id.is_empty())
}

/// How long to wait for Herdr before carrying on without it. This call only
/// improves the order candidates are tried in, so a socket that accepts the
/// connection and then goes quiet must not hold the whole program up.
const TIMEOUT: Duration = Duration::from_millis(1500);

pub fn panes() -> Vec<Pane> {
    let Some(stdout) = run_with_timeout(TIMEOUT) else {
        return Vec::new();
    };
    serde_json::from_slice::<PaneListResponse>(&stdout)
        .map(|response| response.result.panes)
        .unwrap_or_default()
}

fn run_with_timeout(timeout: Duration) -> Option<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    // `Command::output` blocks until the child closes its pipes, so the wait has
    // to happen somewhere it can be abandoned. A thread left behind here costs
    // nothing: the process is about to exit either way.
    std::thread::spawn(move || {
        let result = Command::new("herdr").args(["pane", "list"]).output();
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(output)) if output.status.success() => Some(output.stdout),
        _ => None,
    }
}

/// Codex session ids, most likely first.
///
/// The ordering walks outwards from wherever termcopy was started: this pane,
/// then the focused pane, then the same tab, the same workspace, and finally
/// anything else Herdr knows about.
pub fn ranked_session_ids(panes: &[Pane], from_pane: Option<&str>) -> Vec<String> {
    let origin = from_pane.and_then(|id| panes.iter().find(|pane| pane.pane_id == id));

    let rank = |pane: &Pane| -> u8 {
        if Some(pane.pane_id.as_str()) == from_pane {
            return 0;
        }
        if pane.focused {
            return 1;
        }
        match origin {
            Some(origin) if pane.tab_id == origin.tab_id && !pane.tab_id.is_empty() => 2,
            Some(origin)
                if pane.workspace_id == origin.workspace_id && !pane.workspace_id.is_empty() =>
            {
                3
            }
            _ => 4,
        }
    };

    let mut ranked: Vec<(u8, &str)> = panes
        .iter()
        .filter_map(|pane| pane.codex_session().map(|session| (rank(pane), session)))
        .collect();
    ranked.sort_by_key(|&(rank, _)| rank);

    let mut ids = Vec::with_capacity(ranked.len());
    for (_, session) in ranked {
        let session = session.to_string();
        if !ids.contains(&session) {
            ids.push(session);
        }
    }
    ids
}

/// The session id of a specific pane, for `--pane`.
pub fn session_of(panes: &[Pane], pane_id: &str) -> Option<String> {
    panes
        .iter()
        .find(|pane| pane.pane_id == pane_id)?
        .codex_session()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(
        id: &str,
        tab: &str,
        workspace: &str,
        agent: Option<&str>,
        session: Option<&str>,
    ) -> Pane {
        Pane {
            pane_id: id.to_string(),
            tab_id: tab.to_string(),
            workspace_id: workspace.to_string(),
            focused: false,
            agent: agent.map(str::to_string),
            agent_session: session.map(|value| AgentSession {
                value: Some(value.to_string()),
            }),
            cwd: None,
        }
    }

    fn fixture() -> Vec<Pane> {
        let mut panes = vec![
            pane(
                "pane-same-tab",
                "tab-primary",
                "workspace-primary",
                Some("codex"),
                Some("session-same-tab"),
            ),
            pane(
                "pane-same-workspace",
                "tab-secondary",
                "workspace-primary",
                Some("codex"),
                Some("session-same-workspace"),
            ),
            pane(
                "pane-elsewhere",
                "tab-elsewhere",
                "workspace-secondary",
                Some("codex"),
                Some("session-elsewhere"),
            ),
            pane(
                "pane-self",
                "tab-primary",
                "workspace-primary",
                Some("codex"),
                Some("session-self"),
            ),
            pane(
                "pane-focused",
                "tab-elsewhere",
                "workspace-secondary",
                Some("codex"),
                Some("session-focused"),
            ),
        ];
        panes[4].focused = true;
        panes
    }

    #[test]
    fn ranks_outwards_from_the_pane_it_was_started_in() {
        assert_eq!(
            ranked_session_ids(&fixture(), Some("pane-self")),
            vec![
                "session-self",
                "session-focused",
                "session-same-tab",
                "session-same-workspace",
                "session-elsewhere",
            ]
        );
    }

    #[test]
    fn falls_back_to_the_focused_pane_when_started_outside_herdr() {
        let ranked = ranked_session_ids(&fixture(), None);
        assert_eq!(ranked.first().map(String::as_str), Some("session-focused"));
        assert_eq!(ranked.len(), 5);
    }

    #[test]
    fn ignores_panes_running_another_agent_or_none_at_all() {
        let panes = vec![
            pane(
                "other-agent",
                "tab-1",
                "workspace-1",
                Some("other-agent"),
                Some("other-session"),
            ),
            pane("no-agent", "tab-1", "workspace-1", None, None),
            pane(
                "codex-pane",
                "tab-1",
                "workspace-1",
                Some("codex"),
                Some("codex-session"),
            ),
            pane(
                "codex-without-session",
                "tab-1",
                "workspace-1",
                Some("codex"),
                None,
            ),
        ];
        assert_eq!(ranked_session_ids(&panes, None), vec!["codex-session"]);
    }

    #[test]
    fn lists_a_session_once_even_when_several_panes_share_it() {
        let panes = vec![
            pane(
                "shared-pane-a",
                "tab-1",
                "workspace-1",
                Some("codex"),
                Some("shared"),
            ),
            pane(
                "shared-pane-b",
                "tab-2",
                "workspace-1",
                Some("codex"),
                Some("shared"),
            ),
        ];
        assert_eq!(ranked_session_ids(&panes, None), vec!["shared"]);
    }

    #[test]
    fn resolves_the_session_of_a_named_pane() {
        assert_eq!(
            session_of(&fixture(), "pane-same-tab"),
            Some("session-same-tab".to_string())
        );
        assert_eq!(session_of(&fixture(), "does-not-exist"), None);
    }

    #[test]
    fn parses_a_synthetic_pane_list_response() {
        let response = r#"{"id":"cli:pane:list","result":{"panes":[
            {"agent":"codex","agent_session":{"agent":"codex","kind":"id","source":"herdr:codex","value":"session-current"},
             "agent_status":"idle","cwd":"/fixture/worktree","focused":false,"foreground_cwd":"/fixture/worktree","pane_id":"w1:p1","revision":0,
             "tab_id":"w1:t1","terminal_id":"terminal-json-a","workspace_id":"w1"},
            {"agent_status":"unknown","cwd":"/fixture/worktree","focused":true,"foreground_cwd":"/fixture/worktree","pane_id":"w1:p2","revision":0,
             "tab_id":"w1:t1","terminal_id":"terminal-json-b","workspace_id":"w1"}
        ],"type":"pane_list"}}"#;
        let parsed: PaneListResponse = serde_json::from_str(response).unwrap();
        assert_eq!(parsed.result.panes.len(), 2);
        assert_eq!(
            parsed.result.panes[0].codex_session(),
            Some("session-current")
        );
        assert_eq!(parsed.result.panes[1].codex_session(), None);
    }
}
