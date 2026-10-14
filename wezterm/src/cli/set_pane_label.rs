use crate::cli::UsageError;
use clap::Parser;
use mux::pane::PaneId;
use wezterm_client::client::Client;

#[derive(Debug, Parser, Clone)]
pub struct SetPaneLabel {
    /// The pane to label.
    /// The default is to use the current pane based on the
    /// environment variable WEZTERM_PANE.
    #[arg(long)]
    pane_id: Option<PaneId>,

    /// The pane by name rather than id: its label, its thread's name (a
    /// thread with one pane), or `thread/label`.
    #[arg(long, conflicts_with = "pane_id")]
    pane: Option<String>,

    /// The label. Empty, it takes the pane's label away.
    label: String,
}

impl SetPaneLabel {
    pub async fn run(&self, client: Client) -> anyhow::Result<()> {
        let label = checked_label(&self.label)?;
        let pane_id =
            crate::cli::target::resolve(&client, self.pane_id, self.pane.as_deref()).await?;
        client
            .set_pane_label(codec::PaneLabelChanged { pane_id, label })
            .await?;
        Ok(())
    }
}

/// `text` as the mux keeps a label (see `mux::normalized_pane_label`), with
/// blank meaning none. Digits alone are refused, since `--pane` reads them
/// as a pane id.
pub fn checked_label(text: &str) -> anyhow::Result<Option<String>> {
    let Some(label) = mux::normalized_pane_label(text) else {
        return Ok(None);
    };
    if label.chars().all(|c| c.is_ascii_digit()) {
        return Err(UsageError(format!(
            "{label:?} would read as a pane id; give a label that is not only digits"
        ))
        .into());
    }
    Ok(Some(label))
}

#[cfg(test)]
mod tests {
    use super::checked_label;

    #[test]
    fn labels_are_trimmed_blank_is_none_and_digits_are_refused() {
        assert_eq!(
            checked_label("  reviewer ").unwrap(),
            Some("reviewer".to_string())
        );
        assert_eq!(checked_label("   ").unwrap(), None);
        assert_eq!(
            checked_label("build 2").unwrap(),
            Some("build 2".to_string())
        );
        assert_eq!(
            checked_label("\x1b[31mred").unwrap(),
            Some("[31mred".to_string())
        );
        let err = checked_label("42").unwrap_err();
        assert_eq!(crate::cli::exit_status(&err), 2);
        let err = checked_label("4\x072").unwrap_err();
        assert_eq!(crate::cli::exit_status(&err), 2);
    }
}
