//! herdr backend: drives workspaces through the `herdr` CLI.
//!
//! herdr is a tmux-like agent multiplexer: a background server owns the
//! terminals, clients attach. Its tmux counterparts map as
//! session → workspace, pane → pane, `tmux attach` → `herdr terminal
//! attach <terminal_id>`.
//!
//! All non-attach commands print JSON (verified against herdr 0.9.1):
//!
//! - `herdr workspace list` → `.result.workspaces[]` with `workspace_id`,
//!   `label`, `agent_status`
//! - `herdr workspace create --cwd <dir> --label <label> --no-focus` →
//!   `.result.workspace.workspace_id`, `.result.root_pane.pane_id`
//! - `herdr pane list --workspace <id>` → `.result.panes[]` with `pane_id`,
//!   `terminal_id`, `agent_status`
//! - `herdr pane read <pane_id> --lines N` → plain text (ANSI stripped)
//! - `herdr pane run <pane_id> <text>` → sends `text` + Enter atomically
//! - `herdr tab create --workspace <id> --cwd <dir> --label <label>` →
//!   `.result.root_pane.pane_id` (same shape as workspace create)
//! - `herdr terminal attach <terminal_id>` → foreground attach (detach
//!   ctrl+b q)
//!
//! Pane processes are injected with `HERDR_ENV=1`, `HERDR_WORKSPACE_ID`,
//! `HERDR_TAB_ID` and `HERDR_PANE_ID` — spectatui uses those to tell it is
//! running inside herdr (agents then launch as tabs in the current
//! workspace) and to exclude its own pane from tail selection.
//!
//! Parsing is deliberately lenient (`serde_json::Value` lookups, not strict
//! structs) so herdr adding fields does not break spectatui.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use super::{MuxLaunch, MuxSession, SessionStatus};

pub struct HerdrClient;

/// A workspace entry from `herdr workspace list`.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub label: String,
    pub agent_status: String,
}

/// A pane entry from `herdr pane list`.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub pane_id: String,
    pub terminal_id: String,
    pub agent_status: String,
    pub tab_id: String,
}

/// A herdr tab, parsed from `herdr tab list --workspace <wid>`.
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub tab_id: String,
    pub label: String,
    pub agent_status: String,
}

impl HerdrClient {
    /// The id of the herdr workspace spectatui is running in, when launched
    /// from inside a herdr pane. herdr injects `HERDR_WORKSPACE_ID` into
    /// every pane process.
    pub fn enclosing_workspace_id() -> Option<String> {
        std::env::var("HERDR_WORKSPACE_ID")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// The id of the herdr pane spectatui is running in, when launched from
    /// inside a herdr pane (`HERDR_PANE_ID`). Used to exclude our own pane
    /// when choosing which pane to tail.
    pub fn enclosing_pane_id() -> Option<String> {
        std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|s| !s.is_empty())
    }

    pub async fn has_herdr() -> bool {
        Command::new("herdr")
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Labels of all live workspaces.
    pub async fn list_workspaces() -> Result<Vec<String>> {
        let out = Self::run(&["workspace", "list"]).await?;
        Ok(parse_workspace_list(&out)
            .into_iter()
            .map(|w| w.label)
            .collect())
    }

    /// Find the session (workspace or tab) whose label contains `feature_id`.
    ///
    /// Checks workspace labels first (standalone workspace case), then falls
    /// back to tab labels across all workspaces (in-herdr case where the
    /// agent is launched as a tab in the current workspace). Returns the
    /// matching tab's/workspace's first pane.
    pub async fn find_workspace(feature_id: &str) -> Option<MuxSession> {
        let out = Self::run(&["workspace", "list"]).await.ok()?;
        let workspaces = parse_workspace_list(&out);

        // Pass 1: find by workspace label (standalone session).
        if let Some(workspace) = workspaces.iter().find(|w| w.label.contains(feature_id)) {
            let panes_out = Self::run(&["pane", "list", "--workspace", &workspace.workspace_id])
                .await
                .ok()?;
            let pane = parse_pane_list(&panes_out).into_iter().next()?;
            return Some(MuxSession {
                name: workspace.label.clone(),
                pane_id: pane.pane_id,
                attach_target: pane.terminal_id,
                status: herdr_status(&pane.agent_status),
                last_snapshot: Vec::new(),
            });
        }

        // Pass 2: find by tab label (in-herdr agent-as-tab).
        for workspace in &workspaces {
            let tab_out = Self::run(&["tab", "list", "--workspace", &workspace.workspace_id])
                .await
                .ok()?;
            if let Some(tab) = parse_tab_list(&tab_out)
                .iter()
                .find(|t| t.label.contains(feature_id))
            {
                // Get the first pane belonging to this tab.
                let panes_out = Self::run(&["pane", "list", "--workspace", &workspace.workspace_id])
                    .await
                    .ok()?;
                let pane = parse_pane_list(&panes_out)
                    .into_iter()
                    .find(|p| p.tab_id == tab.tab_id)?;
                return Some(MuxSession {
                    name: tab.label.clone(),
                    pane_id: pane.pane_id,
                    attach_target: pane.terminal_id,
                    status: herdr_status(&pane.agent_status),
                    last_snapshot: Vec::new(),
                });
            }
        }

        None
    }

    /// Find the *project* workspace — the one whose label is exactly
    /// `label` (the configured prefix + the project directory name, e.g.
    /// `spectatui-spectatui`) — and select its agent pane to tail.
    ///
    /// This is the "one workspace per project" layout: the workspace holds
    /// an agent tab plus the spectatui dashboard tab, so the pane to tail
    /// is chosen by [`choose_agent_pane`].
    pub async fn find_project_workspace(label: &str) -> Option<MuxSession> {
        let out = Self::run(&["workspace", "list"]).await.ok()?;
        let workspace = parse_workspace_list(&out)
            .into_iter()
            .find(|w| w.label == label)?;

        let panes_out = Self::run(&["pane", "list", "--workspace", &workspace.workspace_id])
            .await
            .ok()?;
        let panes = parse_pane_list(&panes_out);
        let self_pane = Self::enclosing_pane_id();
        let pane = choose_agent_pane(&panes, self_pane.as_deref())?;

        Some(MuxSession {
            name: workspace.label,
            pane_id: pane.pane_id.clone(),
            attach_target: pane.terminal_id.clone(),
            status: herdr_status(&pane.agent_status),
            last_snapshot: Vec::new(),
        })
    }

    /// Read the last `lines` lines of a pane. When `color` is true the output
    /// retains ANSI escape sequences (`--ansi`), otherwise it is stripped.
    pub async fn read_pane(pane_id: &str, lines: u16, color: bool) -> Result<Vec<String>> {
        let lines_arg = lines.to_string();
        let mut args = vec!["pane", "read", pane_id, "--lines", &lines_arg];
        if color {
            args.push("--ansi");
        }
        let out = Self::run(&args)
            .await
            .context("failed to read herdr pane")?;
        Ok(out.lines().map(|l| l.to_string()).collect())
    }

    /// Send `text` followed by Enter to the pane (herdr does both atomically).
    pub async fn run_in_pane(pane_id: &str, text: &str) -> Result<()> {
        Self::run(&["pane", "run", pane_id, text])
            .await
            .context("failed to send keys to herdr pane")?;
        Ok(())
    }

    /// Create a new tab labeled `label` in `cwd` inside workspace
    /// `workspace_id`, returning the tab's root pane id.
    pub async fn create_tab(workspace_id: &str, cwd: &Path, label: &str) -> Result<String> {
        let out = Self::run(&[
            "tab",
            "create",
            "--workspace",
            workspace_id,
            "--cwd",
            cwd.to_str().unwrap_or("."),
            "--label",
            label,
        ])
        .await
        .context("failed to create herdr tab")?;
        parse_root_pane_id(&out).context("herdr tab create returned no root pane")
    }

    /// Create a session running `command` in `cwd`, labeled `label`.
    ///
    /// When spectatui itself runs inside a herdr pane, the agent is created
    /// as a new *tab* in the current workspace (the user is already in
    /// herdr and can switch to it — no attach, hence `MuxLaunch.attach`
    /// is `false`). Otherwise a new workspace is created unfocused and the
    /// caller attaches to it.
    pub async fn launch_workspace(label: &str, cwd: &Path, command: &str) -> Result<MuxLaunch> {
        if let Some(workspace_id) = Self::enclosing_workspace_id() {
            let pane_id = Self::create_tab(&workspace_id, cwd, label).await?;
            Self::run(&["pane", "run", &pane_id, command])
                .await
                .context("failed to start command in herdr tab")?;
            return Ok(MuxLaunch {
                name: label.to_string(),
                attach: false,
            });
        }

        let out = Self::run(&[
            "workspace",
            "create",
            "--cwd",
            cwd.to_str().unwrap_or("."),
            "--label",
            label,
            "--no-focus",
        ])
        .await
        .context("failed to create herdr workspace")?;

        let pane_id = parse_root_pane_id(&out)
            .context("herdr workspace create returned no root pane")?;
        Self::run(&["pane", "run", &pane_id, command])
            .await
            .context("failed to start command in herdr workspace")?;
        Ok(MuxLaunch {
            name: label.to_string(),
            attach: true,
        })
    }

    /// Attach to a terminal as a foreground process with inherited stdio.
    ///
    /// The caller must leave the alternate screen / raw mode before calling
    /// this and restore it after the future resolves (on detach, ctrl+b q).
    pub async fn attach(terminal_id: &str) -> Result<()> {
        Command::new("herdr")
            .args(["terminal", "attach", terminal_id])
            .status()
            .await
            .context("failed to attach to herdr terminal")?;
        Ok(())
    }

    /// Run a herdr subcommand, returning stdout as text. Fails on a
    /// non-zero exit (herdr prints a JSON error object to stdout in that
    /// case, which is surfaced in the error context).
    async fn run(args: &[&str]) -> Result<String> {
        let output = Command::new("herdr")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("failed to run herdr")?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = if stdout.trim().is_empty() {
                stderr.trim().to_string()
            } else {
                stdout.trim().to_string()
            };
            Err(anyhow::anyhow!(
                "herdr {} failed: {detail}",
                args.first().copied().unwrap_or("")
            ))
        }
    }
}

/// Map a herdr `agent_status` to spectatui's session status.
///
/// herdr reports `working`/`blocked` while an agent is active; `idle`/`done`
/// (and `unknown`, i.e. a plain shell with no detected agent) mean no work
/// is in flight — the same "shell sitting there" case tmux detects via
/// `pane_current_command`.
pub fn herdr_status(agent_status: &str) -> SessionStatus {
    match agent_status {
        "working" | "blocked" => SessionStatus::Running,
        _ => SessionStatus::Idle,
    }
}

/// Choose which pane to tail inside a project workspace: skip spectatui's
/// own pane (`self_pane`), prefer a pane whose agent is working/blocked, then
/// fall back to the first remaining pane.
pub fn choose_agent_pane<'a>(
    panes: &'a [PaneInfo],
    self_pane: Option<&str>,
) -> Option<&'a PaneInfo> {
    let candidates: Vec<&PaneInfo> = panes
        .iter()
        .filter(|p| self_pane.map_or(true, |s| p.pane_id != s))
        .collect();
    candidates
        .iter()
        .find(|p| matches!(herdr_status(&p.agent_status), SessionStatus::Running))
        .or_else(|| candidates.first())
        .copied()
}

/// Parse `herdr workspace list` output. Tolerates a missing/odd shape by
/// returning an empty list.
pub fn parse_workspace_list(json: &str) -> Vec<WorkspaceInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(workspaces) = value
        .get("result")
        .and_then(|r| r.get("workspaces"))
        .and_then(|w| w.as_array())
    else {
        return Vec::new();
    };
    workspaces
        .iter()
        .filter_map(|w| {
            Some(WorkspaceInfo {
                workspace_id: w.get("workspace_id")?.as_str()?.to_string(),
                label: w.get("label")?.as_str()?.to_string(),
                agent_status: w
                    .get("agent_status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            })
        })
        .collect()
}

/// Parse `herdr pane list` output.
pub fn parse_pane_list(json: &str) -> Vec<PaneInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(panes) = value
        .get("result")
        .and_then(|r| r.get("panes"))
        .and_then(|p| p.as_array())
    else {
        return Vec::new();
    };
    panes
        .iter()
        .filter_map(|p| {
            Some(PaneInfo {
                pane_id: p.get("pane_id")?.as_str()?.to_string(),
                terminal_id: p.get("terminal_id")?.as_str()?.to_string(),
                agent_status: p
                    .get("agent_status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                tab_id: p.get("tab_id")?.as_str()?.to_string(),
            })
        })
        .collect()
}

/// Parse a herdr create response (`workspace create` / `tab create`),
/// returning the root pane id. Both share the `.result.root_pane` shape.
pub fn parse_root_pane_id(json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
    value
        .get("result")
        .and_then(|r| r.get("root_pane"))
        .and_then(|p| p.get("pane_id"))
        .and_then(|id| id.as_str())
        .map(|s| s.to_string())
}

/// Parse `herdr tab list --workspace <wid>` output. Tolerates a missing/odd
/// shape by returning an empty list.
pub fn parse_tab_list(json: &str) -> Vec<TabInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(tabs) = value
        .get("result")
        .and_then(|r| r.get("tabs"))
        .and_then(|t| t.as_array())
    else {
        return Vec::new();
    };
    tabs.iter()
        .filter_map(|t| {
            Some(TabInfo {
                tab_id: t.get("tab_id")?.as_str()?.to_string(),
                label: t.get("label")?.as_str()?.to_string(),
                agent_status: t
                    .get("agent_status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_list() {
        let json = r#"{
            "id": "cli:workspace:list",
            "result": {
                "type": "workspace_list",
                "workspaces": [
                    {"active_tab_id": "w2:t1", "agent_status": "unknown",
                     "focused": true, "label": "~", "number": 1,
                     "pane_count": 1, "tab_count": 1, "workspace_id": "w2"},
                    {"agent_status": "working", "focused": false,
                     "label": "spectatui-001-auth", "number": 2,
                     "workspace_id": "w3"}
                ]
            }
        }"#;
        let workspaces = parse_workspace_list(json);
        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].workspace_id, "w2");
        assert_eq!(workspaces[0].label, "~");
        assert_eq!(workspaces[1].label, "spectatui-001-auth");
        assert_eq!(workspaces[1].agent_status, "working");
    }

    #[test]
    fn parses_pane_list() {
        let json = r#"{
            "id": "cli:pane:list",
            "result": {
                "panes": [
                    {"agent_status": "unknown", "cwd": "/tmp",
                     "pane_id": "w3:p1", "terminal_id": "term_abc",
                     "workspace_id": "w3", "tab_id": "w3:t1"}
                ],
                "type": "pane_list"
            }
        }"#;
        let panes = parse_pane_list(json);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "w3:p1");
        assert_eq!(panes[0].terminal_id, "term_abc");
    }

    #[test]
    fn parses_created_root_pane() {
        let json = r#"{
            "id": "cli:workspace:create",
            "result": {
                "root_pane": {"pane_id": "w3:p1",
                              "terminal_id": "term_65c84fe56132f3",
                              "workspace_id": "w3"},
                "type": "workspace_created",
                "workspace": {"workspace_id": "w3", "label": "spectatui-x"}
            }
        }"#;
        assert_eq!(parse_root_pane_id(json).as_deref(), Some("w3:p1"));
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse_workspace_list("not json").is_empty());
        assert!(parse_pane_list("null").is_empty());
        assert!(parse_root_pane_id("{}").is_none());
    }

    #[test]
    fn choose_agent_pane_prefers_working_and_skips_self() {
        let panes = vec![
            PaneInfo {
                pane_id: "w5:p1".into(),
                terminal_id: "term_a".into(),
                agent_status: "unknown".into(),
                tab_id: "w5:t1".into(),
            },
            PaneInfo {
                pane_id: "w5:p2".into(),
                terminal_id: "term_b".into(),
                agent_status: "working".into(),
                tab_id: "w5:t2".into(),
            },
            PaneInfo {
                pane_id: "w5:p3".into(),
                terminal_id: "term_c".into(),
                agent_status: "idle".into(),
                tab_id: "w5:t3".into(),
            },
        ];
        // Prefers the working pane over the first (shell) pane.
        assert_eq!(choose_agent_pane(&panes, None).unwrap().pane_id, "w5:p2");
        // Its own pane is excluded.
        assert_eq!(
            choose_agent_pane(&panes, Some("w5:p1")).unwrap().pane_id,
            "w5:p2"
        );
        // When the working pane is our own, fall back to the first other pane.
        assert_eq!(
            choose_agent_pane(&panes, Some("w5:p2")).unwrap().pane_id,
            "w5:p1"
        );
        // Only our own pane → nothing to tail.
        let only_self = vec![panes[1].clone()];
        assert!(choose_agent_pane(&only_self, Some("w5:p2")).is_none());
        // Empty list → none.
        assert!(choose_agent_pane(&[], None).is_none());
    }

    #[test]
    fn status_mapping() {
        assert_eq!(herdr_status("working"), SessionStatus::Running);
        assert_eq!(herdr_status("blocked"), SessionStatus::Running);
        assert_eq!(herdr_status("idle"), SessionStatus::Idle);
        assert_eq!(herdr_status("done"), SessionStatus::Idle);
        assert_eq!(herdr_status("unknown"), SessionStatus::Idle);
    }
}
