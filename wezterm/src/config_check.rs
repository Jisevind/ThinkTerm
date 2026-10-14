//! `thinkterm config check`: load a configuration file as the app would,
//! in this process alone, and say what is wrong with it.

use clap::Parser;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser, Clone)]
pub struct ConfigCommand {
    #[command(subcommand)]
    sub: ConfigSubCommand,
}

#[derive(Debug, Parser, Clone)]
enum ConfigSubCommand {
    /// Load a configuration file and report its errors and warnings
    /// without applying it: the running app is left alone. Exits with
    /// status 1 when the file does not load.
    #[command(name = "check")]
    Check(CheckCommand),
}

#[derive(Debug, Parser, Clone)]
struct CheckCommand {
    /// The file to check, a draft for instance. The default is the file the
    /// app would load.
    path: Option<PathBuf>,
}

impl ConfigCommand {
    pub fn run(&self, opts: &crate::Opt) -> anyhow::Result<()> {
        match &self.sub {
            ConfigSubCommand::Check(cmd) => cmd.run(opts),
        }
    }
}

impl CheckCommand {
    fn run(&self, opts: &crate::Opt) -> anyhow::Result<()> {
        if opts.skip_config {
            return Err(crate::cli::UsageError(
                "-n leaves the configuration out: there is nothing to check".to_string(),
            )
            .into());
        }
        let path = self
            .path
            .as_deref()
            .or_else(|| opts.config_file.as_deref().map(Path::new));
        if let Some(path) = path {
            config::set_config_file_override(path);
        }
        // `--config name=value` too, as the app would take it.
        config::set_config_overrides(&opts.config_override)?;
        let loaded = config::Config::load();
        let name = loaded
            .file_name
            .as_ref()
            .map(|path| path.display().to_string());
        for warning in &loaded.warnings {
            eprintln!("warning: {warning}");
        }
        match loaded.config {
            Ok(_) => {
                let mut out = std::io::stdout().lock();
                match &name {
                    Some(name) if loaded.warnings.is_empty() => writeln!(out, "{name}: ok"),
                    Some(name) => writeln!(
                        out,
                        "{name}: loads, with {} warning{}",
                        loaded.warnings.len(),
                        if loaded.warnings.len() == 1 { "" } else { "s" }
                    ),
                    None => writeln!(
                        out,
                        "no configuration file to check; the app goes by its defaults \
                         and what its Settings window saved"
                    ),
                }
                .map_err(crate::stdout_error)
            }
            Err(err) => {
                if format!("{err:#}").contains("field 'gui'") {
                    eprintln!(
                        "note: wezterm.gui is there only in the app; the mux server loads \
                         this file without it too, and fails the same way. Use it inside \
                         `if wezterm.gui then ... end`."
                    );
                }
                Err(match name {
                    Some(name) => err.context(name),
                    None => err,
                })
            }
        }
    }
}
