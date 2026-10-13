use crate::cli::agent::state_label;
use crate::cli::CliOutputFormatKind;
use anyhow::{bail, Context};
use clap::Parser;
use codec::Pdu;
use mux::pane::PaneId;
use std::io::Write;
use std::time::Duration;
use thinkterm_proto::{TabId, WindowId};
use wezterm_client::client::Client;
use wezterm_term::{Alert, Progress};

/// How many events may wait unprinted before further ones are dropped.
const BACKLOG: usize = 4096;
/// How often a quiet stream checks that the server is still there, and
/// that the connection has not been replaced by a reconnect.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser, Clone)]
pub struct EventsCommand {
    /// Only what happens to this pane; events about tabs and windows are
    /// left out.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// "table" prints a line per event; "json" prints one JSON object per
    /// line, with a "type" and the event's own fields.
    #[arg(long = "format", default_value = "table")]
    format: CliOutputFormatKind,
}

// One line of `--format json` each: a stable output format. Add events and
// fields; do not remove or retype them.
#[derive(serde::Serialize, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    PaneAdded {
        pane_id: PaneId,
    },
    PaneRemoved {
        pane_id: PaneId,
    },
    /// The program the pane started ended.
    ProgramExited {
        pane_id: PaneId,
        exit_code: Option<u32>,
        signal: Option<String>,
    },
    /// The pane's shell reported a command finished (OSC 133).
    CommandFinished {
        pane_id: PaneId,
        exit_code: Option<i32>,
    },
    Bell {
        pane_id: PaneId,
    },
    Notification {
        pane_id: PaneId,
        title: Option<String>,
        body: String,
    },
    TitleChanged {
        pane_id: PaneId,
        title: String,
    },
    CwdChanged {
        pane_id: PaneId,
    },
    Progress {
        pane_id: PaneId,
        state: &'static str,
        percent: Option<u8>,
    },
    UserVar {
        pane_id: PaneId,
        name: String,
        value: String,
    },
    ClipboardWritten {
        pane_id: PaneId,
    },
    AgentStatus {
        pane_id: PaneId,
        agent: Option<String>,
        state: Option<&'static str>,
    },
    ForegroundProgram {
        pane_id: PaneId,
        executable: Option<String>,
        runs: Option<String>,
    },
    PaneFocused {
        pane_id: PaneId,
    },
    TabAdded {
        tab_id: TabId,
        window_id: WindowId,
    },
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceChanged {
        window_id: WindowId,
        workspace: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

impl Event {
    /// The event a server push stands for, if it stands for one.
    fn from_pdu(pdu: Pdu) -> Option<Self> {
        Some(match pdu {
            Pdu::PaneAdded(codec::PaneAdded { pane_id }) => Self::PaneAdded { pane_id },
            Pdu::PaneRemoved(codec::PaneRemoved { pane_id }) => Self::PaneRemoved { pane_id },
            Pdu::PaneExited(codec::PaneExited { pane_id, exit }) => Self::ProgramExited {
                pane_id,
                exit_code: exit.code,
                signal: exit.signal,
            },
            Pdu::NotifyAlert(codec::NotifyAlert { pane_id, alert }) => match alert {
                Alert::Bell => Self::Bell { pane_id },
                Alert::ToastNotification { title, body, .. } => Self::Notification {
                    pane_id,
                    title,
                    body,
                },
                Alert::WindowTitleChanged(title) => Self::TitleChanged { pane_id, title },
                Alert::CurrentWorkingDirectoryChanged => Self::CwdChanged { pane_id },
                Alert::Progress(progress) => {
                    let (state, percent) = match progress {
                        Progress::None => ("none", None),
                        Progress::Percentage(p) => ("percentage", Some(p)),
                        Progress::Error(p) => ("error", Some(p)),
                        Progress::Indeterminate => ("indeterminate", None),
                    };
                    Self::Progress {
                        pane_id,
                        state,
                        percent,
                    }
                }
                Alert::SetUserVar { name, value } => Self::UserVar {
                    pane_id,
                    name,
                    value,
                },
                Alert::CommandFinished { exit_code } => {
                    Self::CommandFinished { pane_id, exit_code }
                }
                Alert::IconTitleChanged(_)
                | Alert::TabTitleChanged(_)
                | Alert::PaletteChanged
                | Alert::OutputSinceFocusLost => return None,
            },
            // What was copied is not repeated here.
            Pdu::SetClipboard(codec::SetClipboard { pane_id, .. }) => {
                Self::ClipboardWritten { pane_id }
            }
            Pdu::AgentStatusChanged(codec::AgentStatusChanged { pane_id, status }) => {
                Self::AgentStatus {
                    pane_id,
                    state: status.as_ref().map(|status| state_label(status.state)),
                    agent: status.map(|status| status.agent_id),
                }
            }
            Pdu::ForegroundProgramChanged(codec::ForegroundProgramChanged { pane_id, program }) => {
                Self::ForegroundProgram {
                    pane_id,
                    runs: program.as_ref().and_then(|program| program.runs.clone()),
                    executable: program.map(|program| program.executable),
                }
            }
            Pdu::PaneFocused(codec::PaneFocused { pane_id }) => Self::PaneFocused { pane_id },
            Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id }) => {
                Self::TabAdded { tab_id, window_id }
            }
            Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title }) => {
                Self::TabTitleChanged { tab_id, title }
            }
            Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title }) => {
                Self::WindowTitleChanged { window_id, title }
            }
            Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                window_id,
                workspace,
            }) => Self::WorkspaceChanged {
                window_id,
                workspace,
            },
            Pdu::RenameWorkspace(codec::RenameWorkspace {
                old_workspace,
                new_workspace,
            }) => Self::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            },
            _ => return None,
        })
    }

    fn pane_id(&self) -> Option<PaneId> {
        match self {
            Self::PaneAdded { pane_id }
            | Self::PaneRemoved { pane_id }
            | Self::ProgramExited { pane_id, .. }
            | Self::CommandFinished { pane_id, .. }
            | Self::Bell { pane_id }
            | Self::Notification { pane_id, .. }
            | Self::TitleChanged { pane_id, .. }
            | Self::CwdChanged { pane_id }
            | Self::Progress { pane_id, .. }
            | Self::UserVar { pane_id, .. }
            | Self::ClipboardWritten { pane_id }
            | Self::AgentStatus { pane_id, .. }
            | Self::ForegroundProgram { pane_id, .. }
            | Self::PaneFocused { pane_id } => Some(*pane_id),
            Self::TabAdded { .. }
            | Self::TabTitleChanged { .. }
            | Self::WindowTitleChanged { .. }
            | Self::WorkspaceChanged { .. }
            | Self::WorkspaceRenamed { .. } => None,
        }
    }

    /// The `--format table` line, without its time.
    fn describe(&self) -> String {
        let or_none = |value: &Option<String>| value.clone().unwrap_or_else(|| "-".to_string());
        match self {
            Self::PaneAdded { pane_id } => format!("pane_added          pane {pane_id}"),
            Self::PaneRemoved { pane_id } => format!("pane_removed        pane {pane_id}"),
            Self::ProgramExited {
                pane_id,
                exit_code,
                signal,
            } => match signal {
                Some(signal) => format!("program_exited      pane {pane_id}  {signal}"),
                None => match exit_code {
                    Some(code) => format!("program_exited      pane {pane_id}  status {code}"),
                    None => format!("program_exited      pane {pane_id}  status unknown"),
                },
            },
            Self::CommandFinished { pane_id, exit_code } => match exit_code {
                Some(code) => format!("command_finished    pane {pane_id}  status {code}"),
                None => format!("command_finished    pane {pane_id}"),
            },
            Self::Bell { pane_id } => format!("bell                pane {pane_id}"),
            Self::Notification {
                pane_id,
                title,
                body,
            } => format!(
                "notification        pane {pane_id}  {}{body}",
                title.as_ref().map(|t| format!("{t}: ")).unwrap_or_default()
            ),
            Self::TitleChanged { pane_id, title } => {
                format!("title_changed       pane {pane_id}  {title}")
            }
            Self::CwdChanged { pane_id } => format!("cwd_changed         pane {pane_id}"),
            Self::Progress {
                pane_id,
                state,
                percent,
            } => match percent {
                Some(p) => format!("progress            pane {pane_id}  {state} {p}%"),
                None => format!("progress            pane {pane_id}  {state}"),
            },
            Self::UserVar {
                pane_id,
                name,
                value,
            } => format!("user_var            pane {pane_id}  {name}={value}"),
            Self::ClipboardWritten { pane_id } => {
                format!("clipboard_written   pane {pane_id}")
            }
            Self::AgentStatus {
                pane_id,
                agent,
                state,
            } => format!(
                "agent_status        pane {pane_id}  {} {}",
                or_none(agent),
                state.unwrap_or("-")
            ),
            Self::ForegroundProgram {
                pane_id,
                executable,
                runs,
            } => match runs {
                Some(runs) => format!(
                    "foreground_program  pane {pane_id}  {} ({runs})",
                    or_none(executable)
                ),
                None => format!(
                    "foreground_program  pane {pane_id}  {}",
                    or_none(executable)
                ),
            },
            Self::PaneFocused { pane_id } => format!("pane_focused        pane {pane_id}"),
            Self::TabAdded { tab_id, window_id } => {
                format!("tab_added           tab {tab_id}  window {window_id}")
            }
            Self::TabTitleChanged { tab_id, title } => {
                format!("tab_title_changed   tab {tab_id}  {title}")
            }
            Self::WindowTitleChanged { window_id, title } => {
                format!("window_title_changed  window {window_id}  {title}")
            }
            Self::WorkspaceChanged {
                window_id,
                workspace,
            } => format!("workspace_changed   window {window_id}  {workspace}"),
            Self::WorkspaceRenamed {
                old_workspace,
                new_workspace,
            } => format!("workspace_renamed   {old_workspace} -> {new_workspace}"),
        }
    }
}

/// Text a program in a pane chose (a title, a user var, a notification),
/// made safe to print to a terminal: control characters are written out
/// rather than sent, so none of them reaches the terminal running this.
fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

impl EventsCommand {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let pushes = wezterm_client::client::tap_unilateral_pdus(BACKLOG);
        client.subscribe_events(codec::SubscribeEvents {}).await?;
        let mut subscribed_on = client.connection_generation();

        let out = std::io::stdout();
        loop {
            // A reconnect is a new connection on the server, which has
            // not been asked for events.
            if client.connection_generation() != subscribed_on {
                subscribed_on = client.connection_generation();
                client.subscribe_events(codec::SubscribeEvents {}).await?;
            }
            let next = smol::future::or(async { Some(pushes.recv().await) }, async {
                smol::Timer::after(LIVENESS_INTERVAL).await;
                None
            })
            .await;
            let pdu = match next {
                Some(Ok(pdu)) => pdu,
                Some(Err(_)) => bail!("the event stream ended"),
                None => {
                    client
                        .ping()
                        .await
                        .context("lost the connection to the server")?;
                    continue;
                }
            };
            let Some(event) = Event::from_pdu(pdu) else {
                continue;
            };
            if self.pane_id.is_some() && event.pane_id() != self.pane_id {
                continue;
            }
            let line = match self.format {
                CliOutputFormatKind::Json => serde_json::to_string(&event)?,
                CliOutputFormatKind::Table => format!(
                    "{}  {}",
                    chrono::Local::now().format("%H:%M:%S"),
                    printable(&event.describe())
                ),
            };
            writeln!(out.lock(), "{line}").map_err(crate::stdout_error)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_program_exit_status_is_explicit() {
        let event = Event::from_pdu(Pdu::PaneExited(codec::PaneExited {
            pane_id: 4,
            exit: thinkterm_proto::PaneExit {
                code: None,
                signal: None,
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"type":"program_exited","pane_id":4,"exit_code":null,"signal":null}"#
        );
        assert!(event.describe().ends_with("status unknown"));
    }

    #[test]
    fn pushes_become_events_and_json_keeps_its_shape() {
        let exited = Event::from_pdu(Pdu::PaneExited(codec::PaneExited {
            pane_id: 4,
            exit: thinkterm_proto::PaneExit {
                code: Some(2),
                signal: None,
            },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&exited).unwrap(),
            r#"{"type":"program_exited","pane_id":4,"exit_code":2,"signal":null}"#
        );

        let finished = Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
            pane_id: 4,
            alert: Alert::CommandFinished { exit_code: Some(1) },
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&finished).unwrap(),
            r#"{"type":"command_finished","pane_id":4,"exit_code":1}"#
        );

        let bell = Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
            pane_id: 9,
            alert: Alert::Bell,
        }))
        .unwrap();
        assert_eq!(bell, Event::Bell { pane_id: 9 });
        assert_eq!(bell.pane_id(), Some(9));
    }

    #[test]
    fn table_lines_carry_no_control_characters() {
        let event = Event::UserVar {
            pane_id: 1,
            name: "x".to_string(),
            value: "\x1b]52;c;cHduZWQ=\x07\u{9b}2J".to_string(),
        };
        let line = printable(&event.describe());
        assert!(!line.chars().any(char::is_control), "{:?}", line);
        assert!(line.contains("\\u{1b}]52;c;cHduZWQ="), "{:?}", line);
    }

    #[test]
    fn noise_and_clipboard_contents_stay_out() {
        assert_eq!(
            Event::from_pdu(Pdu::NotifyAlert(codec::NotifyAlert {
                pane_id: 1,
                alert: Alert::OutputSinceFocusLost,
            })),
            None
        );
        let copied = Event::from_pdu(Pdu::SetClipboard(codec::SetClipboard {
            pane_id: 1,
            clipboard: Some("secret".to_string()),
            selection: wezterm_term::ClipboardSelection::Clipboard,
        }))
        .unwrap();
        assert!(!serde_json::to_string(&copied).unwrap().contains("secret"));
    }
}
