#![cfg(unix)]
//! With Settings keeping pane output, a Thread's panes come back from a
//! server exit with what they showed: kept at the exit, put back above the
//! new shell when the Thread is opened again, and spent.

use anyhow::{Context, Result};
use codec::{DecodedPdu, Pdu};
use config::keyassignment::SpawnTabDomain;
use mux::domain::{Domain, LocalDomain};
use mux::kept_output::{self, Owner};
use mux::localpane::LocalPane;
use mux::pane::Pane;
use mux::Mux;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wezterm_mux_server_impl::sessionhandler::{ConnectionPeer, PduSender, SessionHandler};
use wezterm_mux_server_impl::thinkterm_layout;

struct Client {
    handler: SessionHandler,
    replies: smol::channel::Receiver<DecodedPdu>,
    serial: u64,
}

impl Client {
    fn new() -> Self {
        let (tx, replies) = smol::channel::unbounded();
        Self {
            handler: SessionHandler::for_peer(
                PduSender::new(move |pdu| {
                    let mut bytes = vec![];
                    pdu.pdu.encode(&mut bytes, pdu.serial)?;
                    tx.try_send(Pdu::decode(bytes.as_slice())?)?;
                    Ok(())
                }),
                ConnectionPeer::Local,
            ),
            replies,
            serial: 0,
        }
    }

    async fn call(&mut self, pdu: Pdu) -> Result<Pdu> {
        self.serial += 1;
        let mut bytes = vec![];
        pdu.encode(&mut bytes, self.serial)?;
        self.handler.process_one(Pdu::decode(bytes.as_slice())?);
        loop {
            let response = self.replies.recv().await?;
            if response.serial == self.serial {
                return Ok(response.pdu);
            }
        }
    }

    async fn open_thread(&mut self, thread_id: Option<String>) -> Result<(String, String)> {
        match self
            .call(Pdu::EnsureThinkTermThread(codec::EnsureThinkTermThread {
                preferred_thread_id: thread_id,
                size: wezterm_term::TerminalSize::default(),
            }))
            .await?
        {
            Pdu::EnsureThinkTermThreadResponse(landed) => Ok((landed.thread_id, landed.workspace)),
            other => anyhow::bail!("unexpected reply {:?}", other),
        }
    }
}

fn panes_in(mux: &Mux, workspace: &str) -> Vec<Arc<dyn Pane>> {
    mux.iter_windows_in_workspace(workspace)
        .into_iter()
        .filter_map(|window_id| {
            let window = mux.get_window(window_id)?;
            Some(
                window
                    .iter()
                    .flat_map(|tab| tab.iter_all_panes())
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn rows(pane: &Arc<dyn Pane>) -> Vec<String> {
    pane.downcast_ref::<LocalPane>()
        .unwrap()
        .primary_screen_lines(1000)
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect()
}

async fn until(pane: &Arc<dyn Pane>, done: impl Fn(&[String]) -> bool) -> Result<Vec<String>> {
    for _ in 0..200 {
        let rows = rows(pane);
        if done(&rows) {
            return Ok(rows);
        }
        smol::Timer::after(Duration::from_millis(50)).await;
    }
    anyhow::bail!("the pane never got there: {:?}", rows(pane))
}

fn separator(rows: &[String]) -> Option<usize> {
    rows.iter().position(|row| row.starts_with("── closed "))
}

async fn scenario() -> Result<()> {
    let mux = Mux::get();
    let mut client = Client::new();
    let (thread_id, workspace) = client.open_thread(None).await?;
    let pane = panes_in(&mux, &workspace)
        .pop()
        .context("the Thread opened with a shell")?;
    pane.writer()
        .write_all(b"echo kept-marker; echo API_TOKEN=abcdefgh12345678\n")?;
    until(&pane, |rows| {
        rows.iter().any(|row| row == "API_TOKEN=abcdefgh12345678")
    })
    .await?;

    // The server's exit: the layout, then what its panes show.
    thinkterm_layout::flush_now()?;
    thinkterm_layout::keep_pane_output();
    // Folder names keep a workspace name's letters, digits, `-` and `_`.
    let folder: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let kept = config::DATA_DIR
        .join("kept-output")
        .join("server")
        .join(folder);
    assert_eq!(std::fs::read_dir(&kept)?.count(), 1);
    // Nothing on disk says what the token was.
    let packed = std::fs::read(kept.join(pane.pane_id().to_string()))?;
    let plain = String::from_utf8(zstd::decode_all(packed.as_slice())?)?;
    assert!(plain.contains("kept-marker"));
    assert!(!plain.contains("abcdefgh12345678"));

    // Its terminals end with it.
    for pane in panes_in(&mux, &workspace) {
        mux.remove_pane(pane.pane_id());
    }
    assert!(panes_in(&mux, &workspace).is_empty());

    let (reopened, _) = client.open_thread(Some(thread_id.clone())).await?;
    assert_eq!(reopened, thread_id);
    let restored = panes_in(&mux, &workspace)
        .pop()
        .context("the Thread was restored")?;
    // The new shell's first output lands below what came back.
    let rows = until(&restored, |rows| {
        separator(rows).is_some_and(|at| rows[at + 1..].iter().any(|row| !row.is_empty()))
    })
    .await?;
    let marker = rows.iter().position(|row| row == "kept-marker").unwrap();
    let token = rows
        .iter()
        .position(|row| row == &format!("API_TOKEN={}", "•".repeat(16)))
        .unwrap();
    let separator = separator(&rows).unwrap();
    assert!(marker < token && token < separator, "{:?}", rows);
    assert!(rows[separator].contains(" · restored "), "{:?}", rows);
    // Spent: a later exit keeps afresh.
    assert!(!kept.exists());

    // The app rebuilding a Thread of the local session server, as after the
    // server was stopped under it: it names the record in its spawn, and
    // the server's local domain reads it.
    let restored_id = restored.pane_id();
    thinkterm_layout::flush_now()?;
    thinkterm_layout::keep_pane_output();
    for pane in panes_in(&mux, &workspace) {
        mux.remove_pane(pane.pane_id());
    }
    let command = kept_output::with_seed(Owner::Server, &workspace, restored_id, None);
    assert!(command.is_some());
    let (_, rebuilt, _) = mux
        .spawn_tab_or_window(
            None,
            SpawnTabDomain::DefaultDomain,
            command,
            None,
            wezterm_term::TerminalSize::default(),
            None,
            workspace.clone(),
            None,
        )
        .await?;
    let rows = until(&rebuilt, |rows| {
        let separators: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.starts_with("── closed "))
            .map(|(at, _)| at)
            .collect();
        separators.len() == 2 && rows[separators[1] + 1..].iter().any(|row| !row.is_empty())
    })
    .await?;
    assert!(rows.iter().any(|row| row == "kept-marker"), "{:?}", rows);
    kept_output::finish(&workspace);
    assert!(!kept.exists());
    Ok(())
}

#[test]
fn a_thread_comes_back_with_what_its_panes_showed() {
    // This integration binary has one test and owns its process environment.
    let home = tempfile::Builder::new()
        .prefix("tt-kept-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = home.path().canonicalize().unwrap();
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        std::env::set_var(key, &path);
    }
    std::env::remove_var("SSH_AUTH_SOCK");
    mux::kept_output::set_enabled(true).unwrap();
    config::use_test_configuration();
    let mut cfg = (*config::configuration()).clone();
    cfg.mux_enable_ssh_agent = false;
    cfg.default_prog = Some(vec!["/bin/sh".into()]);
    config::use_this_configuration(cfg);
    let local: Arc<dyn Domain> = Arc::new(LocalDomain::new("local").unwrap());
    let mux = Arc::new(Mux::new(Some(local)));
    Mux::set_mux(&mux);
    let executor = promise::spawn::SimpleExecutor::new();
    let result = Arc::new(Mutex::new(None));
    let completed = result.clone();
    promise::spawn::spawn(async move {
        let outcome = smol::future::or(scenario(), async {
            smol::Timer::after(Duration::from_secs(30)).await;
            anyhow::bail!("the kept output scenario timed out")
        })
        .await;
        *completed.lock().unwrap() = Some(outcome);
    })
    .detach();
    while result.lock().unwrap().is_none() {
        executor.tick().unwrap();
    }
    for pane in mux.iter_panes() {
        mux.remove_pane(pane.pane_id());
    }
    result.lock().unwrap().take().unwrap().unwrap();
}
