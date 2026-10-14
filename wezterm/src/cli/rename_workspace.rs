use clap::Parser;
use mux::pane::PaneId;
use std::collections::HashMap;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct RenameWorkspace {
    /// Specify the workspace to rename
    #[arg(long)]
    workspace: Option<String>,

    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    ///
    /// The pane is used to figure out which workspace
    /// should be renamed.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// The new name for the workspace
    new_workspace: String,
}

impl RenameWorkspace {
    pub async fn run(self, client: Client) -> anyhow::Result<()> {
        let panes = client.list_panes().await?;

        let mut pane_id_to_workspace = HashMap::new();

        for tabroot in panes.tabs {
            let mut cursor = tabroot.into_tree().cursor();

            loop {
                if let Some(stack) = cursor.leaf_mut() {
                    for entry in &stack.panes {
                        pane_id_to_workspace.insert(entry.pane_id, entry.workspace.to_string());
                    }
                }
                match cursor.preorder_next() {
                    Ok(c) => cursor = c,
                    Err(_) => break,
                }
            }
        }

        let old_workspace = if let Some(workspace) = self.workspace {
            workspace
        } else {
            // Find the current tab from the pane id
            let pane_id =
                crate::cli::target::resolve(&client, self.pane_id, self.pane.as_deref()).await?;
            pane_id_to_workspace
                .get(&pane_id)
                .ok_or_else(|| crate::cli::no_such_pane(pane_id))?
                .to_string()
        };

        client
            .rename_workspace(codec::RenameWorkspace {
                old_workspace,
                new_workspace: self.new_workspace,
            })
            .await?;
        Ok(())
    }
}
