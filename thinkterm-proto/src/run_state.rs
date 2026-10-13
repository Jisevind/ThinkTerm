//! How far a pane's programs have got.
//!
//! Whether the program the pane started is still running or how it ended,
//! and -- when the pane's shell reports it with OSC 133 -- how many
//! commands it has finished and how the last one ended. `thinkterm cli
//! wait` and `send-key --wait` read it; the mux that owns the pane answers.
//!
//! Varbincode is positional: **field order is the wire contract**. Bump the
//! codec version when changing anything here.

use serde::{Deserialize, Serialize};

/// How the program a pane started ended.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PaneExit {
    /// The exit code. A program ended by a signal has the code its
    /// platform reports for that (1 on unix). None means the process
    /// ended but its status is unavailable, as after live handoff.
    pub code: Option<u32>,
    /// The signal that ended it, when one did.
    pub signal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProgramState {
    Running,
    Exited(PaneExit),
    /// This kind of pane cannot say, or it ended without its status being
    /// seen.
    Unknown,
}

/// What the pane's shell has said with OSC 133 about its commands.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellCommands {
    /// The shell has sent a mark, so the rest of this means something.
    pub reported: bool,
    /// Commands finished since the first mark.
    pub finished: u64,
    /// A command has started and not yet finished.
    pub running: bool,
    /// The status the last finished command reported.
    pub last_exit: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PaneRunState {
    pub program: ProgramState,
    pub shell: ShellCommands,
}

impl PaneRunState {
    /// What a pane that knows nothing about its programs reports.
    pub fn unknown() -> Self {
        Self {
            program: ProgramState::Unknown,
            shell: ShellCommands::default(),
        }
    }
}
