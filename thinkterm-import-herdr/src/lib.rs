//! Herdr's saved-layout and live-handoff adapter. No mux or GUI dependency.
//! Herdr's Windows server cannot hand its terminals over, so Windows imports
//! the saved layout only.

mod model;
#[cfg(unix)]
mod seed;
mod source;
#[cfg(unix)]
mod transport;

#[cfg(unix)]
use anyhow::Context;
use anyhow::{ensure, Result};
#[cfg(unix)]
use model::Runtime;
use model::Snapshot;
use thinkterm_import::{
    ImportContext, ImportMode, ImportPlan, ImportRequest, ImportSource, PreparedImport, Preview,
    Project, Selection, Session, SourceInfo, Thread,
};
#[cfg(unix)]
use thinkterm_import::{LiveTerminal, TerminalSeed};

struct Herdr;
static HERDR: Herdr = Herdr;

pub fn register() -> Result<()> {
    thinkterm_import::register(&HERDR)
}

impl ImportSource for Herdr {
    fn info(&self) -> SourceInfo {
        SourceInfo {
            id: "herdr",
            name: "Herdr",
            icon: "herdr",
        }
    }

    fn discover(&self, context: &ImportContext) -> Result<Vec<Session>> {
        source::discover(context)
    }

    fn preview(&self, context: &ImportContext, session: &str) -> Result<Preview> {
        source::preview(context, session)
    }

    fn prepare(&self, context: &ImportContext, request: &ImportRequest) -> Result<PreparedImport> {
        let preview = source::preview(context, &request.session)?;
        ensure!(
            preview.live == request.mode.is_live() && preview.fingerprint == request.fingerprint,
            "Herdr changed since the preview; inspect the session again"
        );
        ensure!(
            preview.unavailable.is_none(),
            "{}",
            preview.unavailable.unwrap_or_default()
        );
        let dir = source::session_dir(context, &request.session)?;
        if request.mode == ImportMode::Layout {
            let (snapshot, fingerprint) = source::saved_snapshot(&dir)?;
            ensure!(
                fingerprint == request.fingerprint,
                "Herdr layout changed; inspect it again"
            );
            snapshot.check_working_directories()?;
            return Ok(PreparedImport {
                plan: into_plan(snapshot, request.mode),
                terminals: vec![],
                handoff: None,
            });
        }
        receive_live(context, &dir, request.mode)
    }

    fn notes(&self, mode: Option<ImportMode>) -> Vec<&'static str> {
        let mut notes = vec![];
        #[cfg(windows)]
        notes.push("herdr-windows-layout-only");
        if cfg!(unix) && mode != Some(ImportMode::Layout) {
            notes.push("herdr-requirements-live");
            #[cfg(target_os = "linux")]
            notes.push("herdr-requirements-linux");
        }
        if mode == Some(ImportMode::Live) {
            notes.push("herdr-live-limits");
        }
        notes.push("herdr-keep-state");
        notes
    }

    fn result_notes(&self) -> Vec<&'static str> {
        vec!["herdr-keep-state"]
    }

    fn error_key(&self, message: &str) -> Option<&'static str> {
        error_key(message)
    }

    fn run_helper(&self) -> Option<Result<()>> {
        #[cfg(unix)]
        return transport::maybe_run_relay();
        #[cfg(windows)]
        None
    }
}

#[cfg(unix)]
fn receive_live(
    context: &ImportContext,
    dir: &std::path::Path,
    mode: ImportMode,
) -> Result<PreparedImport> {
    let mut transfer =
        transport::receive(dir, &context.executable).context("Receive Herdr handoff")?;
    let plan = into_plan(transfer.manifest.snapshot.clone(), mode);
    let terminals = transfer
        .manifest
        .panes
        .iter()
        .zip(transfer.fds.drain(..))
        .map(|(runtime, pty)| LiveTerminal {
            pane_id: runtime.pane_id,
            seed: terminal_seed(runtime),
            pty,
        })
        .collect();
    Ok(PreparedImport {
        plan,
        terminals,
        handoff: Some(Box::new(transfer)),
    })
}

#[cfg(windows)]
fn receive_live(_: &ImportContext, _: &std::path::Path, _: ImportMode) -> Result<PreparedImport> {
    anyhow::bail!("Herdr on Windows cannot hand over running terminals")
}

#[cfg(unix)]
impl thinkterm_import::Handoff for transport::Transfer {
    fn commit(&mut self) -> Result<()> {
        transport::Transfer::commit(self)
    }
    fn finish(self: Box<Self>) {
        transport::Transfer::finish(*self);
    }
}

#[cfg(unix)]
fn terminal_seed(runtime: &Runtime) -> TerminalSeed {
    TerminalSeed {
        child_pid: runtime.child_pid,
        rows: runtime.rows,
        cols: runtime.cols,
        cell_width_px: runtime.cell_width_px,
        cell_height_px: runtime.cell_height_px,
        title: runtime.terminal_title.clone(),
        ansi: seed::seed_ansi(runtime),
    }
}

fn into_plan(snapshot: Snapshot, mode: ImportMode) -> ImportPlan {
    let active = Selection {
        project: snapshot
            .active
            .unwrap_or(0)
            .min(snapshot.workspaces.len().saturating_sub(1)),
        thread: 0,
    };
    let projects = snapshot
        .workspaces
        .into_iter()
        .enumerate()
        .map(|(index, workspace)| {
            let name = workspace.display_name(index);
            let active_tab = workspace
                .active_tab
                .min(workspace.tabs.len().saturating_sub(1));
            let tabs = workspace
                .tabs
                .into_iter()
                .map(|tab| thinkterm_import::Tab {
                    name: tab.custom_name,
                    layout: into_layout(tab.layout),
                    panes: tab
                        .panes
                        .into_iter()
                        .map(|(id, pane)| {
                            (
                                id,
                                thinkterm_import::Pane {
                                    cwd: pane.cwd,
                                    title: pane.label,
                                },
                            )
                        })
                        .collect(),
                    focused: tab.focused,
                    zoomed: tab.zoomed,
                })
                .collect();
            Project {
                name: name.clone(),
                directory: workspace.identity_cwd,
                threads: vec![Thread {
                    name,
                    tabs,
                    active_tab,
                }],
            }
        })
        .collect();
    ImportPlan {
        mode,
        projects,
        active,
    }
}

fn into_layout(layout: model::Layout) -> thinkterm_import::Layout {
    match layout {
        model::Layout::Pane(id) => thinkterm_import::Layout::Pane(id),
        model::Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => thinkterm_import::Layout::Split {
            direction: match direction {
                model::Direction::Horizontal => thinkterm_import::Direction::Horizontal,
                model::Direction::Vertical => thinkterm_import::Direction::Vertical,
            },
            ratio,
            first: Box::new(into_layout(*first)),
            second: Box::new(into_layout(*second)),
        },
    }
}

fn error_key(message: &str) -> Option<&'static str> {
    [
        (
            "This running Herdr server does not support live handoff",
            "herdr-error-live-unsupported",
        ),
        ("Herdr session is empty", "session-import-error-empty"),
        (
            "Herdr session directory is unavailable",
            "session-import-error-session-missing",
        ),
        (
            "A saved Herdr working directory is unavailable",
            "session-import-error-directory-missing",
        ),
        (
            "This Herdr snapshot format is newer than ThinkTerm supports",
            "session-import-error-layout-newer",
        ),
        (
            "Unsupported Herdr handoff version",
            "session-import-error-handoff-version",
        ),
        (
            "Herdr changed since the preview; inspect the session again",
            "session-import-error-changed",
        ),
        (
            "Herdr layout changed; inspect it again",
            "session-import-error-changed",
        ),
        (
            "Herdr session belongs to a different user",
            "session-import-error-local-user",
        ),
        (
            "Invalid Herdr socket owner or type",
            "session-import-error-local-user",
        ),
        (
            "Some Herdr panes have no live terminal; the session was not moved",
            "session-import-error-terminal-missing",
        ),
    ]
    .iter()
    .find_map(|(reason, key)| message.ends_with(reason).then_some(*key))
    .or_else(|| {
        if message.contains("failed to spawn handoff import server at ")
            || message.ends_with("Herdr did not start its handoff receiver")
            || message.ends_with("Herdr ended the handoff without a receiver")
        {
            Some("session-import-error-receiver")
        } else if message.contains("Herdr session directory is unavailable") {
            Some("session-import-error-session-missing")
        } else {
            None
        }
    })
}

#[cfg(all(test, unix))]
mod tests;
