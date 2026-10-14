use crate::cli::activate_pane_direction::PaneDirectionParser;
use clap::Parser;
use config::keyassignment::PaneDirection;
use mux::pane::PaneId;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct GetPaneDirection {
    /// Specify the current pane.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// The direction to consider.
    #[arg(value_parser=PaneDirectionParser{})]
    direction: PaneDirection,
}

impl GetPaneDirection {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let pane_id =
            crate::cli::target::resolve(&client, self.pane_id, self.pane.as_deref()).await?;
        let response = client
            .get_pane_direction(codec::GetPaneDirection {
                pane_id,
                direction: self.direction,
            })
            .await?;
        if let Some(pane_id) = response.pane_id {
            println!("{pane_id}");
        }
        Ok(())
    }
}
