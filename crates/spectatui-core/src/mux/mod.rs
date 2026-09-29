//! Terminal-multiplexer backend abstraction.
//!
//! spectatui drives a per-feature agent session through a terminal
//! multiplexer. Two backends are supported, with the same surface:
//!
//! | action          | tmux                                    | herdr                                  |
//! |-----------------|-----------------------------------------|----------------------------------------|
//! | list sessions   | `tmux list-sessions`                    | `herdr workspace list`                 |
//! | find session    | name contains feature id + `list-panes` | label contains feature id + `pane list`|
//! | capture output  | `capture-pane -p -S -N`                 | `pane read <pane_id> --lines N`        |
//! | send keys       | `send-keys -l <text>` + `Enter`         | `pane run <pane_id> <text>`            |
//! | send keys       | `send-keys -l <text>` + `Enter`         | `pane run <pane_id> <text>`            |
//! | create session  | `new-session -d -s <name> -c <cwd>`     | `workspace create --cwd --label` + run, or `tab create` in the current workspace when spectatui itself runs inside herdr |
//! | attach          | `tmux attach -t <name>`                 | `terminal attach <terminal_id>`        |
//! | find project    | —                                       | exact workspace label + agent-pane pick |
//! | availability    | `tmux -V`                               | `herdr --version`                      |
//!
//! A tmux *session* maps to a herdr *workspace*: the configured session-name
//! prefix (`mux_prefix`) is applied to the tmux session name or the herdr
//! workspace label, so feature lookup works identically for both.
//!
//! Two work layouts exist for herdr: one workspace *per feature* (created
//! by spectatui when it runs in a plain terminal) and one workspace *per
//! project* (label = `mux_prefix` + project directory name, e.g.
//! `spectatui-spectatui`) whose tabs hold an agent plus the spectatui
//! dashboard. The poller matches per-feature labels first and falls back
//! to the project workspace.

pub mod herdr;
pub mod tmux;

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Which terminal multiplexer spectatui drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MuxBackend {
    #[default]
    Tmux,
    Herdr,
}

impl MuxBackend {
    /// Canonical short name, used in UI text ("…live in herdr").
    pub fn as_str(self) -> &'static str {
        match self {
            MuxBackend::Tmux => "tmux",
            MuxBackend::Herdr => "herdr",
        }
    }

    /// Detach key hint shown on the attach screen.
    pub fn detach_hint(self) -> &'static str {
        match self {
            MuxBackend::Tmux => "Ctrl-b d",
            MuxBackend::Herdr => "Ctrl-b q",
        }
    }
}

impl fmt::Display for MuxBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MuxBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "tmux" => Ok(MuxBackend::Tmux),
            "herdr" => Ok(MuxBackend::Herdr),
            other => Err(format!(
                "unknown session backend '{other}' (expected 'tmux' or 'herdr')"
            )),
        }
    }
}

/// Whether a multiplexer session's agent is doing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// The agent (or another non-shell command) is running.
    Running,
    /// The pane is sitting in a shell / agent is done.
    Idle,
    /// The session has exited.
    Exited,
    /// The session no longer exists.
    NotFound,
}

/// A live multiplexer session (one per feature, or the project workspace).
///
/// `name` is the human session name (tmux session name / herdr workspace
/// label). `pane_id` is the capture + send target. `attach_target` is what
/// the foreground attach needs — equal to `name` for tmux, the herdr
/// `terminal_id` for herdr.
#[derive(Debug, Clone)]
pub struct MuxSession {
    pub name: String,
    pub pane_id: String,
    pub attach_target: String,
    pub status: SessionStatus,
    pub last_snapshot: Vec<String>,
}

/// The outcome of creating a new session.
///
/// `attach` is `false` when the session was created inside a workspace the
/// caller is already attached to (a herdr tab opened because spectatui
/// itself runs inside herdr) — the user switches tabs, no foreground
/// attach is needed.
#[derive(Debug, Clone)]
pub struct MuxLaunch {
    pub name: String,
    pub attach: bool,
}

/// Backend-agnostic client facade. Every method takes the backend to drive so
/// the TUI's call sites stay identical across tmux and herdr.
pub struct MuxClient;

impl MuxClient {
    /// Whether the backend's CLI is installed.
    pub async fn is_available(backend: MuxBackend) -> bool {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::has_tmux().await,
            MuxBackend::Herdr => herdr::HerdrClient::has_herdr().await,
        }
    }

    /// Whether the backend supports capturing a pane with ANSI color /
    /// escape sequences (tmux `capture-pane -e`, herdr `pane read --ansi`).
    /// Both backends support it whenever the CLI is installed, so this is
    /// equivalent to availability.
    pub async fn supports_color_capture(backend: MuxBackend) -> bool {
        Self::is_available(backend).await
    }

    /// Names of all live sessions (tmux session names / herdr workspace labels).
    pub async fn list_sessions(backend: MuxBackend) -> Vec<String> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::list_sessions().await.unwrap_or_default(),
            MuxBackend::Herdr => herdr::HerdrClient::list_workspaces().await.unwrap_or_default(),
        }
    }

    /// Find the session whose name contains `feature_id`, with its first pane.
    pub async fn find_session(backend: MuxBackend, feature_id: &str) -> Option<MuxSession> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::find_session(feature_id).await,
            MuxBackend::Herdr => herdr::HerdrClient::find_workspace(feature_id).await,
        }
    }

    /// Capture the last `lines` lines of the session's pane.
    ///
    /// When `color` is true the backend includes ANSI escape sequences
    /// (`-e` for tmux, `--ansi` for herdr); when false the output is plain
    /// text (the historical behavior).
    pub async fn capture_pane(
        backend: MuxBackend,
        pane_id: &str,
        lines: u16,
        color: bool,
    ) -> anyhow::Result<Vec<String>> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::capture_pane(pane_id, lines, color).await,
            MuxBackend::Herdr => herdr::HerdrClient::read_pane(pane_id, lines, color).await,
        }
    }

    /// Send `text` followed by Enter to the session's pane.
    pub async fn send_keys(backend: MuxBackend, pane_id: &str, text: &str) -> anyhow::Result<()> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::send_keys(pane_id, text).await,
            MuxBackend::Herdr => herdr::HerdrClient::run_in_pane(pane_id, text).await,
        }
    }

    /// Create a new session named `name` in `cwd` and start `command` in it.
    ///
    /// Returns [`MuxLaunch`] so the caller knows whether to foreground-attach
    /// (a tab created in the caller's own herdr workspace does not need it).
    pub async fn launch_session(
        backend: MuxBackend,
        name: &str,
        cwd: &Path,
        command: &str,
    ) -> anyhow::Result<MuxLaunch> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::launch_session(name, cwd, command).await,
            MuxBackend::Herdr => herdr::HerdrClient::launch_workspace(name, cwd, command).await,
        }
    }

    /// Find the *project* session — the multiplexer session whose label is
    /// exactly `label` (prefix + project directory name) — with the pane to
    /// tail. A herdr-only concept (the per-project workspace holds agent
    /// tabs); always `None` for tmux.
    pub async fn find_project_session(backend: MuxBackend, label: &str) -> Option<MuxSession> {
        match backend {
            MuxBackend::Tmux => None,
            MuxBackend::Herdr => herdr::HerdrClient::find_project_workspace(label).await,
        }
    }

    /// Foreground-attach to `target` with the inherited terminal.
    ///
    /// For tmux this is the session name; for herdr the `terminal_id`.
    pub async fn attach(backend: MuxBackend, target: &str) -> anyhow::Result<()> {
        match backend {
            MuxBackend::Tmux => tmux::TmuxClient::attach(target).await,
            MuxBackend::Herdr => herdr::HerdrClient::attach(target).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_round_trips() {
        assert_eq!(MuxBackend::Tmux.as_str(), "tmux");
        assert_eq!(MuxBackend::Herdr.as_str(), "herdr");
        assert_eq!("tmux".parse::<MuxBackend>().unwrap(), MuxBackend::Tmux);
        assert_eq!("HERDR".parse::<MuxBackend>().unwrap(), MuxBackend::Herdr);
        assert!("wizard".parse::<MuxBackend>().is_err());
        assert_eq!(MuxBackend::default(), MuxBackend::Tmux);
    }

    #[test]
    fn backend_serde_is_lowercase() {
        assert_eq!(
            serde_json::to_string(&MuxBackend::Herdr).unwrap(),
            "\"herdr\""
        );
        assert_eq!(
            serde_json::from_str::<MuxBackend>("\"tmux\"").unwrap(),
            MuxBackend::Tmux
        );
    }

    #[test]
    fn detach_hints() {
        assert_eq!(MuxBackend::Tmux.detach_hint(), "Ctrl-b d");
        assert_eq!(MuxBackend::Herdr.detach_hint(), "Ctrl-b q");
    }
}
