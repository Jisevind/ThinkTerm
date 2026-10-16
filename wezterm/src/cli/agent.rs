//! `thinkterm cli agent …`: query agent statuses from the mux.

use crate::cli::CliOutputFormatKind;
use clap::Parser;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use tabout::{tabulate_output, Alignment, Column};
use thinkterm_proto::{AgentEvidence, AgentState};
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct AgentCommand {
    #[command(subcommand)]
    sub: AgentSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum AgentSubCommand {
    /// List panes running detected coding agents
    #[command(name = "list")]
    List(AgentListCommand),

    /// Tell a coding agent starting in this pane where it is: its pane and
    /// the others in its tab. The session-start hook `thinkterm skill
    /// setup` installs runs this; outside a pane it prints nothing.
    #[command(name = "context")]
    Context(AgentContextCommand),
}

#[derive(Debug, Parser, Clone)]
struct AgentContextCommand {}

#[derive(Debug, Parser, Clone)]
struct AgentListCommand {
    /// Controls the output format.
    /// "table" and "json" are possible formats.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,

    /// The same as `--format json`.
    #[arg(long, conflicts_with = "format")]
    json: bool,
}

impl AgentCommand {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        match &self.sub {
            AgentSubCommand::List(cmd) => cmd.run(client).await,
            AgentSubCommand::Context(cmd) => cmd.run(client).await,
        }
    }
}

/// A pane as the context describes it.
struct ContextPane {
    pane_id: mux::pane::PaneId,
    tab_id: mux::tab::TabId,
    tab_title: String,
    title: String,
    label: Option<String>,
}

/// At most this many neighbours are named; an agent can list the rest.
const CONTEXT_NEIGHBOURS: usize = 12;
/// Titles past this many characters are cut short.
const CONTEXT_TITLE_CHARS: usize = 80;

impl AgentContextCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let Some(pane_id) = std::env::var("WEZTERM_PANE")
            .ok()
            .and_then(|id| id.parse::<mux::pane::PaneId>().ok())
        else {
            return Ok(());
        };
        let listed = client.list_panes().await?;
        let mut panes = vec![];
        for (tabroot, tab_title) in listed.tabs.into_iter().zip(listed.tab_titles.iter()) {
            let mut cursor = tabroot.into_tree().cursor();
            loop {
                if let Some(stack) = cursor.leaf_mut() {
                    for entry in &stack.panes {
                        panes.push(ContextPane {
                            pane_id: entry.pane_id,
                            tab_id: entry.tab_id,
                            tab_title: tab_title.clone(),
                            title: entry.title.clone(),
                            label: listed.pane_labels.get(&entry.pane_id).cloned(),
                        });
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }
        if let Some(text) = session_context(pane_id, &panes) {
            let out = std::io::stdout();
            write!(out.lock(), "{text}").map_err(crate::stdout_error)?;
        }
        Ok(())
    }
}

/// What an agent starting in `pane_id` is told, or None when that pane is
/// not among `panes` (a pane of another server).
fn session_context(pane_id: mux::pane::PaneId, panes: &[ContextPane]) -> Option<String> {
    let me = panes.iter().find(|pane| pane.pane_id == pane_id)?;
    let mut text = format!("You are running inside ThinkTerm, in pane {}", describe(me));
    if !me.tab_title.is_empty() {
        text.push_str(&format!(" in tab \"{}\"", me.tab_title));
    }
    text.push_str(".\n");
    let beside: Vec<_> = panes
        .iter()
        .filter(|pane| pane.tab_id == me.tab_id && pane.pane_id != pane_id)
        .collect();
    if beside.is_empty() {
        text.push_str("No other pane shares this tab.\n");
    } else {
        text.push_str("Other panes in this tab:\n");
        for pane in beside.iter().take(CONTEXT_NEIGHBOURS) {
            text.push_str(&format!("- pane {}\n", describe(pane)));
        }
        if beside.len() > CONTEXT_NEIGHBOURS {
            text.push_str(&format!("- and {} more\n", beside.len() - CONTEXT_NEIGHBOURS));
        }
    }
    let elsewhere = panes.iter().filter(|pane| pane.tab_id != me.tab_id).count();
    if elsewhere > 0 {
        text.push_str(&format!("Other tabs hold {elsewhere} more.\n"));
    }
    text.push_str(
        "Read a pane's screen with `thinkterm cli get-text --pane <id or label> --trim`; \
         `thinkterm cli list` shows every pane and `thinkterm cli --help` every command. \
         If `thinkterm` is not on PATH, it is in $WEZTERM_EXECUTABLE_DIR.\n",
    );
    Some(text)
}

/// `12 "dev" (pnpm dev)`: the id, the label if any, and the title.
fn describe(pane: &ContextPane) -> String {
    let title: String = pane.title.chars().take(CONTEXT_TITLE_CHARS).collect();
    match &pane.label {
        Some(label) => format!("{} \"{}\" ({})", pane.pane_id, label, title),
        None => format!("{} ({})", pane.pane_id, title),
    }
}

pub(crate) fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Unknown => "unknown",
        AgentState::Error => "error",
    }
}

fn evidence_label(evidence: AgentEvidence) -> &'static str {
    match evidence {
        AgentEvidence::Contract => "contract",
        AgentEvidence::Screen => "screen",
        AgentEvidence::Fallback => "fallback",
        AgentEvidence::Report => "report",
    }
}

// This will be serialized to JSON via the 'List' command.
// As such it is intended to be a stable output format,
// Do not remove or change the types of the fields.
#[derive(serde::Serialize)]
struct CliAgentResultItem {
    pane_id: mux::pane::PaneId,
    /// The label the pane was given, if any.
    label: Option<String>,
    agent: String,
    state: String,
    evidence: String,
    session_id: Option<String>,
    /// Unix seconds of the last state change, on the detecting host's
    /// clock (approximate when that host's clock differs from ours).
    since_unix: u64,
    workspace: String,
    title: String,
}

impl AgentListCommand {
    async fn run(&self, client: Client) -> anyhow::Result<()> {
        let out = std::io::stdout();
        let response = client.get_agent_statuses().await?;
        // Labels are the panes' own, listed with the panes.
        let labels = if response.statuses.is_empty() {
            Default::default()
        } else {
            client.list_panes().await?.pane_labels
        };
        let format = if self.json {
            CliOutputFormatKind::Json
        } else {
            self.format
        };
        match format {
            CliOutputFormatKind::Json => {
                let items: Vec<_> = response
                    .statuses
                    .into_iter()
                    .map(|entry| CliAgentResultItem {
                        label: labels.get(&entry.pane_id).cloned(),
                        pane_id: entry.pane_id,
                        agent: entry.status.agent_id,
                        state: state_label(entry.status.state).to_string(),
                        evidence: evidence_label(entry.status.evidence).to_string(),
                        session_id: entry.status.session_id,
                        since_unix: entry.status.since_unix,
                        workspace: entry.workspace,
                        title: entry.title,
                    })
                    .collect();
                crate::cli::print_json(&items)?;
            }
            CliOutputFormatKind::Table => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let cols = vec![
                    Column {
                        name: "PANEID".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "AGENT".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "STATE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "EVIDENCE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "AGE".to_string(),
                        alignment: Alignment::Right,
                    },
                    Column {
                        name: "WORKSPACE".to_string(),
                        alignment: Alignment::Left,
                    },
                    Column {
                        name: "TITLE".to_string(),
                        alignment: Alignment::Left,
                    },
                    // Last, as in `list`, so the others stay in place.
                    Column {
                        name: "LABEL".to_string(),
                        alignment: Alignment::Left,
                    },
                ];
                let data = response
                    .statuses
                    .iter()
                    .map(|entry| {
                        // Clock skew across hosts renders as a zero age.
                        let age = now.saturating_sub(entry.status.since_unix);
                        vec![
                            entry.pane_id.to_string(),
                            entry.status.agent_id.clone(),
                            state_label(entry.status.state).to_string(),
                            evidence_label(entry.status.evidence).to_string(),
                            format!("{}", humantime::format_duration(
                                std::time::Duration::from_secs(age)
                            )),
                            entry.workspace.clone(),
                            entry.title.clone(),
                            labels.get(&entry.pane_id).cloned().unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                tabulate_output(&cols, &data, &mut out.lock()).map_err(crate::stdout_error)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: mux::pane::PaneId, tab_id: mux::tab::TabId, title: &str, label: Option<&str>) -> ContextPane {
        ContextPane {
            pane_id,
            tab_id,
            tab_title: if tab_id == 3 { "project".into() } else { String::new() },
            title: title.into(),
            label: label.map(str::to_string),
        }
    }

    #[test]
    fn an_agent_is_told_its_pane_and_the_panes_beside_it() {
        let panes = [
            pane(12, 3, "claude", Some("agent")),
            pane(14, 3, "pnpm dev", Some("dev")),
            pane(15, 3, "tail -f app.log", None),
            pane(20, 4, "zsh", None),
        ];
        let text = session_context(12, &panes).unwrap();
        assert!(text.starts_with(
            "You are running inside ThinkTerm, in pane 12 \"agent\" (claude) in tab \"project\".\n"
        ));
        assert!(text.contains("- pane 14 \"dev\" (pnpm dev)\n"));
        assert!(text.contains("- pane 15 (tail -f app.log)\n"));
        assert!(text.contains("Other tabs hold 1 more.\n"));
        assert!(!text.contains("pane 20"));
    }

    #[test]
    fn an_agent_alone_in_its_tab_is_told_so() {
        let text = session_context(20, &[pane(20, 4, "zsh", None)]).unwrap();
        assert!(text.starts_with("You are running inside ThinkTerm, in pane 20 (zsh).\n"));
        assert!(text.contains("No other pane shares this tab.\n"));
        assert!(!text.contains("Other tabs"));
    }

    #[test]
    fn a_pane_of_another_server_gets_nothing() {
        assert!(session_context(99, &[pane(12, 3, "zsh", None)]).is_none());
    }
}
