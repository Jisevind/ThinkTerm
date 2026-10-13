use crate::cli::resolve_relative_cwd;
use clap::{Parser, ValueHint};
use config::keyassignment::SpawnTabDomain;
use config::ConfigHandle;
use mux::pane::PaneId;
use mux::command_spec::{CommandSpec, CommandSpecExt};
use mux::window::WindowId;
use portable_pty::cmdbuilder::CommandBuilder;
use std::ffi::OsString;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SpawnCommand {
    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    /// The pane is used to determine the current domain
    /// and window.
    #[arg(long)]
    pane_id: Option<PaneId>,

    #[arg(long)]
    domain_name: Option<String>,

    /// Specify the window into which to spawn a tab.
    /// If omitted, the window associated with the current
    /// pane is used.
    /// Cannot be used with `--workspace` or `--new-window`.
    #[arg(long, conflicts_with_all=&["workspace", "new_window"])]
    window_id: Option<WindowId>,

    /// Spawn into a new window, rather than a new tab.
    #[arg(long)]
    new_window: bool,

    /// Specify the current working directory for the initially
    /// spawned program
    #[arg(long, value_parser, value_hint=ValueHint::DirPath)]
    cwd: Option<OsString>,

    /// When creating a new window, override the default workspace name
    /// with the provided name.  The default name is "default".
    /// Requires `--new-window`.
    #[arg(long, requires = "new_window")]
    workspace: Option<String>,

    /// Wait for the program to end, and exit with its status. The pane
    /// id is printed first, as without it.
    #[arg(long)]
    wait: bool,

    /// With --wait, give up after this long, exiting with status 124: a
    /// number of seconds, or a number with s, m or h after it.
    #[arg(long, value_parser = crate::cli::wait::parse_timeout, requires = "wait")]
    timeout: Option<std::time::Duration>,

    /// Instead of executing your shell, run PROG.
    /// For example: `thinkterm cli spawn -- bash -l` will spawn bash
    /// as if it were a login shell.
    #[arg(value_parser, value_hint=ValueHint::CommandWithArguments, num_args=1..)]
    prog: Vec<OsString>,
}

impl SpawnCommand {
    pub async fn run(self, client: Client, config: &ConfigHandle) -> anyhow::Result<()> {
        let window_id = if self.new_window {
            None
        } else {
            match self.window_id {
                Some(w) => Some(w),
                None => {
                    let pane_id = client.resolve_pane_id(self.pane_id).await?;

                    let panes = client.list_panes().await?;
                    let mut window_id = None;
                    'outer: for tabroot in panes.tabs {
                        let mut cursor = tabroot.into_tree().cursor();

                        loop {
                            if let Some(stack) = cursor.leaf_mut() {
                                for entry in &stack.panes {
                                    if entry.pane_id == pane_id {
                                        window_id.replace(entry.window_id);
                                        break 'outer;
                                    }
                                }
                            }
                            match cursor.preorder_next() {
                                Ok(c) => cursor = c,
                                Err(_) => break,
                            }
                        }
                    }
                    window_id
                }
            }
        };

        let workspace = self
            .workspace
            .as_deref()
            .unwrap_or(
                config
                    .default_workspace
                    .as_deref()
                    .unwrap_or(mux::DEFAULT_WORKSPACE),
            )
            .to_string();

        let size = config.initial_size(0, None);

        let spawned = client
            .spawn_v2(codec::SpawnV2 {
                domain: self
                    .domain_name
                    .map_or(SpawnTabDomain::DefaultDomain, |name| {
                        SpawnTabDomain::DomainName(name)
                    }),
                window_id,
                command: if self.prog.is_empty() {
                    None
                } else {
                    let builder = CommandBuilder::from_argv(self.prog);
                    Some(CommandSpec::from_command_builder(&builder))
                },
                command_dir: resolve_relative_cwd(self.cwd)?,
                size,
                workspace,
            })
            .await?;

        log::debug!("{:?}", spawned);
        println!("{}", spawned.pane_id);
        if self.wait {
            let deadline = self.timeout.map(|t| std::time::Instant::now() + t);
            let code =
                crate::cli::wait::wait_for_program(&client, spawned.pane_id, deadline).await?;
            crate::cli::wait::exit_with(code);
        }
        Ok(())
    }
}
