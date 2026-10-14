//! `thinkterm cli agent …`: query agent statuses from the mux.

use crate::cli::CliOutputFormatKind;
use clap::Parser;
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
}

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
        }
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
