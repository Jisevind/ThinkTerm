//! Holding the machine's node: what the mux server does, from the time it
//! starts following the settings ([`follow`]). The node starts when the
//! settings turn the tailnet on, restarts when they change what it runs on,
//! and stops when they turn it off; what it does goes to the report the app
//! and `thinkterm tailnet` read.

use crate::node;
use crate::settings::{self, Settings};
use crate::status::{self, read_live_report, read_report, Family, Peer, Report, Status};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often the settings file is looked at for a change made by another
/// process -- the app's switch, `thinkterm tailnet up` -- and how soon a
/// process kept from the node by another one tries again.
const POLL: Duration = Duration::from_secs(3);

/// The longest wait before trying again at a node that would not start.
const RETRY_MAX: Duration = Duration::from_secs(60);

/// How long a connection asked for while this process acts on the settings
/// waits for it to finish.
const SETTLE_WAIT: Duration = Duration::from_secs(5);

type Changed = Arc<dyn Fn() + Send + Sync>;

/// Why the settings thread looks again before its next poll.
enum Wake {
    /// This process changed the settings file.
    Settings,
    /// The node's status or its list of machines changed.
    Node,
}

struct Follower {
    wake: Mutex<mpsc::Sender<Wake>>,
}

static FOLLOWER: OnceLock<Follower> = OnceLock::new();

/// The status while no node runs here: Off, or why there is none.
static IDLE: Mutex<Status> = Mutex::new(Status::Off);

/// How many looks at the settings this process asked its settings thread
/// for -- the first one, and one after each change this process makes --
/// and up to which of them it has acted on. Until it has acted on all, a
/// connection selecting the tailnet waits for the node rather
/// than go out without it: sessions taken over at a restart connect before
/// the first look. Counted, not a flag: a look begun before a change does
/// not act on it, so its end must not end the wait for it.
static ASKED: AtomicU64 = AtomicU64::new(0);
static ACTED_ON: AtomicU64 = AtomicU64::new(0);

/// Whether the settings thread has settings to act on that it has not yet.
fn settling() -> bool {
    ACTED_ON.load(Ordering::SeqCst) < ASKED.load(Ordering::SeqCst)
}

/// Ends every wait for the settings thread however it ends, a panic
/// included: nothing would act on what is asked after that.
struct SettlingEnds;

impl Drop for SettlingEnds {
    fn drop(&mut self) {
        ACTED_ON.store(u64::MAX, Ordering::SeqCst);
    }
}

/// The settings' revision this process last acted on, for its reports.
static ACTED: AtomicU64 = AtomicU64::new(0);

static NODE: Mutex<Option<Arc<node::Node>>> = Mutex::new(None);

/// The nodes in a row this process gave up and started anew (see
/// `node::Node::is_broken`) since one last got in or as far as a login: each
/// waits longer before it is given up.
static GIVEN_UP: AtomicU32 = AtomicU32::new(0);

/// The lock this process holds the machine's node by (see
/// `settings::try_hold_node`), from before its node starts until after the
/// node is gone and said to be.
static HOLD: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Set from `will_hold` until the settings thread first acted on the
/// settings: the process took over from a predecessor that holds the node
/// until it exits.
static TAKING_OVER: AtomicBool = AtomicBool::new(false);

/// How long a connection selecting the tailnet
/// waits for this process to take the node up from its predecessor.
const TAKEOVER_WAIT: Duration = Duration::from_secs(15);

/// This process takes over from a predecessor that holds the node until it
/// exits, and starts following the settings (see `follow`) after that:
/// connections to the tailnet's machines meanwhile wait for this process's
/// own node, rather than go through the predecessor's and end with it.
pub fn will_hold() {
    TAKING_OVER.store(true, Ordering::SeqCst);
}

/// Holds the machine's node in this process for as long as the settings
/// file turns the tailnet on, starting, stopping and restarting it as the
/// file changes. `changed` is called, on a background thread, whenever the
/// file, the status or the list of machines changes. Calling it again does
/// nothing.
pub fn follow(changed: impl Fn() + Send + Sync + 'static) {
    let changed: Changed = Arc::new(changed);
    let (wake, woken) = mpsc::channel();
    if FOLLOWER
        .set(Follower {
            wake: Mutex::new(wake),
        })
        .is_err()
    {
        return;
    }
    ASKED.fetch_add(1, Ordering::SeqCst);
    let spawned = std::thread::Builder::new()
        .name("tailnet-settings".into())
        .spawn(move || {
            let _ends = SettlingEnds;
            follow_loop(changed, woken)
        });
    if let Err(err) = spawned {
        ACTED_ON.store(u64::MAX, Ordering::SeqCst);
        log::error!("tailnet: starting the settings thread: {err}");
    }
}

/// This process changed the settings file: connections asked for from here
/// on wait for the settings thread to act on it, while there is one to.
pub(crate) fn settings_changed() {
    if FOLLOWER.get().is_none() {
        return;
    }
    ASKED.fetch_add(1, Ordering::SeqCst);
    send(Wake::Settings);
}

/// Whether the settings thread got it: not once it has ended.
fn send(wake: Wake) -> bool {
    FOLLOWER
        .get()
        .is_some_and(|follower| follower.wake.lock().unwrap().send(wake).is_ok())
}

/// Whether this process's node was given up (see `node::Node::is_broken`).
fn node_is_broken() -> bool {
    NODE.lock()
        .unwrap()
        .as_ref()
        .is_some_and(|node| node.is_broken())
}

/// This process's node as last seen.
fn status() -> Status {
    if let Some(node) = NODE.lock().unwrap().as_ref() {
        return node.status();
    }
    IDLE.lock().unwrap().clone()
}

/// The other machines in the tailnet, by name.
fn peers() -> Vec<Peer> {
    if let Some(node) = NODE.lock().unwrap().as_ref() {
        return node.peers();
    }
    vec![]
}

/// A connection through this process's node (see crate::dial): Some with
/// its answer while this process holds one, None for the caller to ask the
/// process that does.
pub(crate) fn dial(
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Option<Option<Result<socket2::Socket>>> {
    // A node stopped while this went through it -- restarted for new
    // settings, or given up -- took what it had with it: the one in its
    // place is asked again. Within the one `timeout` either way.
    let started = Instant::now();
    let left = || timeout.saturating_sub(started.elapsed());
    for _ in 0..2 {
        // The caller selected this node, including while settings restart it.
        if settling() {
            if let Err(err) = wait_for_settings(left(), cancelled, settling) {
                return Some(Some(Err(err)));
            }
        }
        let node = NODE.lock().unwrap().clone();
        let Some(node) = node else {
            return dial_once_held(host, port, family, left(), cancelled);
        };
        let dialed = node.dial(host, port, family, left(), cancelled);
        if !node.is_stopped() || cancelled() {
            return Some(dialed);
        }
    }
    Some(None)
}

/// While a predecessor still holds the node (see `will_hold`), the selected
/// connection waits for this process's own node within `timeout`. None once this
/// process is not taking over, or is but holds no node after all, for the
/// caller to ask whichever process holds it.
fn dial_once_held(
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Option<Option<Result<socket2::Socket>>> {
    if !TAKING_OVER.load(Ordering::SeqCst)
        || !settings::load_summary().is_ok_and(|settings| settings.enabled)
    {
        return None;
    }
    let started = Instant::now();
    let deadline = started + timeout.min(TAKEOVER_WAIT);
    loop {
        if let Some(node) = NODE.lock().unwrap().clone() {
            let left = timeout.saturating_sub(started.elapsed());
            return Some(node.dial(host, port, family, left, cancelled));
        }
        if !TAKING_OVER.load(Ordering::SeqCst) {
            return None;
        }
        if cancelled() || Instant::now() >= deadline {
            return Some(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Waits, for up to SETTLE_WAIT, for the settings thread to act on settings
/// it has not yet: a node being started is not a node turned off. The
/// connection goes on after that either way, with whatever node there is --
/// a thread that is slow or gone must not fail every connection, the
/// tailnet's or not. Err only when the session is closed meanwhile.
fn wait_for_settings(
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    settling: impl Fn() -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout.min(SETTLE_WAIT);
    loop {
        if cancelled() {
            anyhow::bail!("the session was closed while waiting for the tailnet settings");
        }
        if !settling() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            log::warn!("tailnet: connecting before the settings thread acted on the settings");
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

/// What the settings thread does after acting on the settings.
enum Next {
    /// Nothing until the settings change.
    Settled,
    /// Another process holds the node: try again at the next poll.
    Held,
    /// It went wrong: try again after a while, a longer one each time.
    Failed,
}

fn follow_loop(changed: Changed, woken: mpsc::Receiver<Wake>) {
    let path = match settings::path() {
        Ok(path) => path,
        Err(err) => {
            log::error!("tailnet: {err:#}");
            return;
        }
    };
    let mut seen = None;
    let mut look = true;
    // When to try again, and how long the wait before that was.
    let mut retry: Option<(Instant, Duration)> = None;
    let mut told = None;
    loop {
        let stamp = settings::stamp(&path);
        let file_changed = look || stamp != seen;
        if file_changed || retry.is_some_and(|(at, _)| Instant::now() >= at) {
            seen = stamp;
            // The looks asked for so far: this one reads the file as they
            // left it.
            let asked = ASKED.load(Ordering::SeqCst);
            retry = match reconcile() {
                Next::Settled => None,
                Next::Held => Some((Instant::now() + POLL, POLL)),
                Next::Failed => {
                    let wait = retry.map_or(POLL, |(_, wait)| (wait * 2).min(RETRY_MAX));
                    Some((Instant::now() + wait, wait))
                }
            };
            ACTED_ON.fetch_max(asked, Ordering::SeqCst);
            // Whatever came of it: holding the node now, or not to.
            TAKING_OVER.store(false, Ordering::SeqCst);
        }
        // Told when it changes, and after every change of the file even when
        // the node goes on as it was: the page shows the file, and
        // `thinkterm tailnet up` waits for a report written after its
        // change.
        let now = (status(), peers());
        if matches!(now.0, Status::Connected { .. } | Status::NeedsLogin { .. }) {
            GIVEN_UP.store(0, Ordering::SeqCst);
        }
        if file_changed || told.as_ref() != Some(&now) {
            report(&now);
            changed();
            told = Some(now);
        }
        look = match woken.recv_timeout(POLL) {
            Ok(Wake::Settings) => true,
            // A node that has to get in again may take an auth key saved
            // while it was in, which did not restart it then; one given up
            // is started anew at once, even one that got in at the last
            // moment, not at whatever look comes next.
            Ok(Wake::Node) => node_is_broken() || !matches!(status(), Status::Connected { .. }),
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
    }
}

/// Starts, stops or restarts this process's node to match the settings.
///
/// The slow parts -- writing new keys, building the node's runtime -- run
/// without the NODE lock, which a dial takes.
fn reconcile() -> Next {
    // Without the secrets first: most looks change nothing.
    let summary = match settings::load_summary() {
        Ok(summary) => summary,
        Err(err) => return failed(err),
    };
    ACTED.store(summary.revision, Ordering::SeqCst);
    if !summary.enabled {
        let node = NODE.lock().unwrap().take();
        let stopped = node.is_some();
        if let Some(node) = node {
            node.stop();
        }
        let was = std::mem::replace(&mut *IDLE.lock().unwrap(), Status::Off);
        let held = HOLD.lock().unwrap().is_some();
        if held && (stopped || matches!(was, Status::Failed { .. })) {
            report_if_ours(Status::Off);
        }
        // Only after the report: the process that takes the node over can
        // only start once this lock is free, so its report comes after.
        HOLD.lock().unwrap().take();
        return Next::Settled;
    }

    {
        let mut hold = HOLD.lock().unwrap();
        if hold.is_none() {
            match settings::try_hold_node() {
                Ok(Some(lock)) => *hold = Some(lock),
                Ok(None) => {
                    drop(hold);
                    *IDLE.lock().unwrap() = Status::HeldElsewhere;
                    return Next::Held;
                }
                Err(err) => {
                    drop(hold);
                    return failed(err);
                }
            }
        }
    }

    let settings = match settings::load() {
        Ok(settings) => settings,
        Err(err) => return failed(err),
    };
    // The machine's identity is made on its first start and kept from then
    // on, so that it stays one node across restarts. Made only while the
    // file still turns the tailnet on -- not after a Log out landed since
    // the file was read -- and the node then starts on the file as it is.
    let settings = if settings.node_keys.is_some() {
        settings
    } else {
        match settings_with_node_keys() {
            Ok(Some(settings)) => settings,
            // Turned off meanwhile: the look the change brings acts on it.
            Ok(None) => return Next::Settled,
            Err(err) => return failed(err),
        }
    };
    let Some(node_keys) = settings.node_keys.clone() else {
        return failed(anyhow::anyhow!("the node's keys were not saved"));
    };
    // Compared in the one spelling, an address written by hand included.
    let control_url = match settings
        .control_url
        .as_deref()
        .map(crate::control_url_as_kept)
    {
        Some(Ok(url)) => url,
        Some(Err(err)) => return unusable(err),
        None => None,
    };
    let config = node::NodeConfig {
        control_url,
        hostname: settings.hostname.clone(),
        auth_key: settings.auth_key.clone(),
        node_keys,
        forward_ssh: settings.serve_ssh,
    };
    if let Err(err) = config.check() {
        return unusable(err);
    }
    let (restart, broken) = match NODE.lock().unwrap().as_ref() {
        Some(node) if node.is_broken() => (true, true),
        Some(node) => {
            let restart = node
                .config()
                .needs_restart_for(&config, &node.status(), node.was_in());
            (restart, false)
        }
        None => (true, false),
    };
    if !restart {
        return Next::Settled;
    }
    if broken {
        GIVEN_UP.fetch_add(1, Ordering::SeqCst);
    }
    // One node on these keys at a time: the old one goes first.
    let old = NODE.lock().unwrap().take();
    if let Some(old) = old {
        old.stop();
    }
    match node::Node::start(
        config,
        node::broken_after(GIVEN_UP.load(Ordering::SeqCst)),
        Box::new(|| {
            send(Wake::Node);
        }),
    ) {
        Ok(node) => {
            *NODE.lock().unwrap() = Some(Arc::new(node));
            *IDLE.lock().unwrap() = Status::Off;
            Next::Settled
        }
        Err(err) => failed(err),
    }
}

/// Settings no node can start on -- the file edited by hand -- leave a
/// running one as it was, rather than take the machine off the tailnet
/// until someone gets to it some other way. Not tried again until the file
/// changes.
fn unusable(err: anyhow::Error) -> Next {
    failed(err.context("not acting on the tailnet settings"));
    Next::Settled
}

/// Says why there is no node; one already running goes on as it was.
fn failed(err: anyhow::Error) -> Next {
    log::warn!("tailnet: {err:#}");
    if NODE.lock().unwrap().is_none() {
        *IDLE.lock().unwrap() = Status::Failed {
            error: format!("{err:#}"),
        };
    }
    Next::Failed
}

/// Writes new keys to the settings file while it still turns the tailnet
/// on, unless another process got there first, and returns the settings as
/// the file holds them now. None once the tailnet is off.
fn settings_with_node_keys() -> Result<Option<Settings>> {
    let mut made = None;
    let settings = crate::update(|settings| {
        if settings.enabled && settings.node_keys.is_none() {
            settings.node_keys = node::new_node_keys().map_err(|err| made = Some(err)).ok();
        }
    })?;
    if !settings.enabled {
        return Ok(None);
    }
    if let Some(err) = made {
        return Err(err);
    }
    Ok(Some(settings))
}

/// Writes what the node is doing for the app and `thinkterm tailnet` to
/// read: this process's node, or why it has none -- held elsewhere, failed
/// before it got the node -- unless another live process reports here.
fn report((status, peers): &(Status, Vec<Peer>)) {
    let holding = HOLD.lock().unwrap().is_some();
    if !holding && read_live_report().is_some_and(|report| report.pid != std::process::id()) {
        return;
    }
    // Nothing to say where the tailnet has never been on.
    if !holding && *status == Status::Off && read_report().is_none() {
        return;
    }
    status::write_report(&Report {
        pid: std::process::id(),
        status: status.clone(),
        peers: peers.clone(),
        revision: ACTED.load(Ordering::SeqCst),
    });
}

/// Says this process's node is gone, unless the report is already another
/// process's.
fn report_if_ours(status: Status) {
    if read_report().is_some_and(|report| report.pid != std::process::id()) {
        return;
    }
    status::write_report(&Report {
        pid: std::process::id(),
        status,
        peers: vec![],
        revision: ACTED.load(Ordering::SeqCst),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialing_waits_for_settings_to_finish_initializing() {
        let settling = AtomicBool::new(true);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(75));
                settling.store(false, Ordering::SeqCst);
            });
            wait_for_settings(Duration::from_secs(1), &|| false, || {
                settling.load(Ordering::SeqCst)
            })
            .unwrap();
            assert!(!settling.load(Ordering::SeqCst));
        });
    }

    #[test]
    fn a_closed_session_stops_waiting_and_a_long_wait_goes_on() {
        assert!(wait_for_settings(Duration::from_secs(1), &|| true, || true).is_err());
        // A settings thread that is slow or gone fails no connection.
        assert!(wait_for_settings(Duration::from_millis(20), &|| false, || true).is_ok());
        assert!(wait_for_settings(Duration::ZERO, &|| false, || false).is_ok());
    }
}
