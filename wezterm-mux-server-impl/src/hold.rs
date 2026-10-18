//! After a handoff the old server stays, as a process that only waits,
//! until everything it started has ended (macOS).
//!
//! macOS lets a program reach the local network by asking who is
//! responsible for it: for everything in a pane, the server that spawned
//! the pane's shell, which is responsible for itself (see
//! `exec_as_own_tcc_identity` in the server binary). The answer holds only
//! while that process runs, and only while it is the build found at its
//! path, the one the Local Network privilege was granted to. A server that
//! exited after handing its panes over left every pane it had spawned
//! unable to reach the local network until the pane was closed: after each
//! update, every pane opened before it. Apple's own programs (ssh, curl,
//! nc) are let through regardless, which made it easy to miss.
//!
//! So once the successor owns the panes, the old server does not exit. It
//! replaces itself with the program at its path -- the new build, after an
//! update -- in a mode that holds on to nothing (ptys, sockets, the pid file
//! and the ports are the successor's) and only waits: for its children,
//! reaping each as it ends, as the successor tells that a pane's process
//! ended by it being gone; and for every process descended from it, which
//! macOS still counts as its, including ones that outlived their shell. It
//! replaces itself again whenever the program at its path changes, so that
//! it stays the build on disk, and exits once none of them runs.

use procinfo::{LocalProcessInfo, LocalProcessStatus};
use rustix::process::{kill_process, test_kill_process, wait, Pid, Signal, WaitOptions};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// What a server runs itself with after a handoff: this, then
/// [`Watch::to_arg`], and nothing else. Every build that may hold reads it
/// the same way: an older holder replaces itself with whatever build is
/// installed now.
const HOLD_OPTION: &str = "--hold-after-handoff";
/// Its name on its command line: not the server's, which scripts match to
/// tell whether a server still runs (install.sh's "still running" notice).
/// macOS goes by the program itself, not this name.
const HOLDER_NAME: &str = "thinkterm-mux-holder";

/// How often the holder looks at what still runs and at its program.
const LOOK_EVERY: Duration = Duration::from_secs(3);
/// How long a program told the server is its parent has to end after being
/// asked to, before it is made to.
const HELPER_GRACE: Duration = Duration::from_secs(2);

/// The processes a holder waits for, by pid and start time (seconds): by
/// the time it looks again a pid may name another process.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Watch(HashMap<u32, u64>);

impl Watch {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    /// `pid:start,pid:start`, oldest first.
    fn to_arg(&self) -> String {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(pid, start)| (**start, **pid));
        entries
            .into_iter()
            .map(|(pid, start)| format!("{pid}:{start}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Reads [`Watch::to_arg`], skipping what it cannot read: a process
    /// missed is one that loses the local network sooner, not a reason to
    /// stop waiting for the rest.
    fn from_arg(arg: &str) -> Self {
        Self(
            arg.split(',')
                .filter_map(|entry| {
                    let (pid, start) = entry.split_once(':')?;
                    Some((pid.trim().parse().ok()?, start.trim().parse().ok()?))
                })
                .filter(|(pid, _)| *pid > 1)
                .collect(),
        )
    }
}

/// A process as the holder sees it: when it started, and the processes
/// descended from it, with theirs.
struct Seen {
    start: u64,
    below: Vec<(u32, u64)>,
}

/// What runs now of what `watch` waited for, plus everything descended
/// from `me`: processes leave `me`'s tree when the parent that started them
/// exits, but they stay `me`'s to macOS, so each one watched before is
/// looked up on its own, and kept while it runs under the same start time
/// -- with whatever it started since.
fn look(watch: &Watch, me: u32, tree: &dyn Fn(u32) -> Option<Seen>) -> Watch {
    // A table this process is not in could not be read: nothing is known
    // to have ended.
    let Some(mine) = tree(me) else {
        return watch.clone();
    };
    let mut next: HashMap<u32, u64> = mine.below.into_iter().collect();
    // Oldest first: an ancestor looked up brings its descendants along,
    // and they need no lookup of their own.
    let mut before: Vec<_> = watch.0.iter().map(|(pid, start)| (*pid, *start)).collect();
    before.sort_by_key(|(pid, start)| (*start, *pid));
    for (pid, start) in before {
        if pid == me || next.contains_key(&pid) {
            continue;
        }
        if let Some(seen) = tree(pid) {
            if seen.start == start {
                next.insert(pid, start);
                next.extend(seen.below);
            }
        }
    }
    next.remove(&me);
    Watch(next)
}

/// `pid` and what descends from it, from the process table; zombies are
/// processes that have ended.
fn process_tree(pid: u32) -> Option<Seen> {
    fn ended(info: &LocalProcessInfo) -> bool {
        matches!(info.status, LocalProcessStatus::Zombie | LocalProcessStatus::Dead)
    }
    fn walk(info: &LocalProcessInfo, below: &mut Vec<(u32, u64)>) {
        for child in info.children.values() {
            if !ended(child) {
                below.push((child.pid, child.start_time));
            }
            walk(child, below);
        }
    }
    // Most of what left the holder's tree has ended: not worth a read of
    // the whole table each.
    let id = Pid::from_raw(i32::try_from(pid).ok()?)?;
    if test_kill_process(id) == Err(rustix::io::Errno::SRCH) {
        return None;
    }
    let info = LocalProcessInfo::with_root_pid(pid)?;
    if ended(&info) {
        return None;
    }
    let mut below = Vec::new();
    walk(&info, &mut below);
    Some(Seen {
        start: info.start_time,
        below,
    })
}

/// Whether `argv` is that of a program told `parent` is its parent, which
/// ends when that process does (`thinkterm-tailnet --parent <pid>`).
fn told_parent_is(argv: &[String], parent: u32) -> bool {
    let parent = parent.to_string();
    argv.windows(2)
        .any(|pair| pair[0] == "--parent" && pair[1] == parent)
}

/// The children of `me` told it is their parent. They end with the server,
/// and a holder is the same process: they would run on next to the
/// successor's own -- two tailnet nodes on one machine's keys.
fn helpers(me: u32) -> Vec<u32> {
    LocalProcessInfo::with_root_pid(me)
        .map(|info| {
            info.children
                .values()
                .filter(|child| told_parent_is(&child.argv, me))
                .map(|child| child.pid)
                .collect()
        })
        .unwrap_or_default()
}

fn signal(pid: u32, signal: Signal) {
    if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        let _ = kill_process(pid, signal);
    }
}

/// Ends the helpers. The successor starts its own once the handoff stream
/// has closed (see `wait_for_predecessor_exit`), as this process became
/// the holder; its node waits for the lock until this one's is gone.
fn stop_helpers(me: u32) {
    let helpers = helpers(me);
    if helpers.is_empty() {
        return;
    }
    for &pid in &helpers {
        signal(pid, Signal::TERM);
    }
    let give_up = Instant::now() + HELPER_GRACE;
    let running = |pid: u32| process_tree(pid).is_some();
    while Instant::now() < give_up && helpers.iter().any(|&pid| running(pid)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    for &pid in &helpers {
        if running(pid) {
            log::warn!("helper {pid} did not end when asked to; killing it");
            signal(pid, Signal::KILL);
        }
    }
}

/// Called once the successor owns every pane, in place of exiting: become
/// the holder for what this server started. At once: until then this is
/// still a server, accepting on the socket the successor accepts on too,
/// and a client reconnecting after the takeover could reach it. Returns
/// only if the program cannot be run, and the caller exits as before.
pub fn hold_instead_of_exiting() {
    match std::env::current_exe() {
        Ok(program) => replace_self(&program, &Watch::default()),
        Err(err) => {
            log::error!("not staying for the panes' processes: no program to run: {err:#}")
        }
    }
}

/// Runs `program` in this process as the holder of `watch`. Returns only
/// when that failed.
fn replace_self(program: &Path, watch: &Watch) {
    let err = std::process::Command::new(program)
        .arg0(HOLDER_NAME)
        .arg(HOLD_OPTION)
        .arg(watch.to_arg())
        .exec();
    log::error!("could not run {} as the holder: {err:#}", program.display());
}

/// Its children are this process's to reap, and the successor tells that a
/// pane's process ended by it being gone: one left a zombie would keep its
/// pane open.
fn reap_children() {
    loop {
        // Any child: each pane's shell leads a process group of its own.
        match wait(WaitOptions::empty()) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            // None left, and a holder starts none.
            Err(_) => return,
        }
    }
}

/// When the program at a path was last replaced, as far as a holder can
/// tell without reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: i64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp {
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        modified: meta.mtime(),
    })
}

/// The watch this process was run to hold for, when it was.
pub fn requested() -> Option<String> {
    // As they are: a server's arguments need not be text.
    let mut args = std::env::args_os().skip(1);
    if args.next()?.as_os_str() != std::ffi::OsStr::new(HOLD_OPTION) {
        return None;
    }
    Some(args.next().and_then(|arg| arg.into_string().ok()).unwrap_or_default())
}

/// The holder, from [`requested`]. Never returns.
pub fn run(arg: &str, start_logging: impl FnOnce()) -> ! {
    // Whatever the server had open is the successor's now, or nobody's: a
    // pty still open here is one its pane's close would not hang up. The
    // log opens after, as its own.
    portable_pty::unix::close_random_fds();
    start_logging();
    if let Err(err) = std::thread::Builder::new()
        .name("hold-reaper".into())
        .spawn(reap_children)
    {
        log::error!("cannot reap the panes' processes, so not holding on: {err:#}");
        std::process::exit(0);
    }
    let me = std::process::id();
    stop_helpers(me);
    let program = std::env::current_exe().ok();
    let mut watch = look(&Watch::from_arg(arg), me, &process_tree);
    if watch.is_empty() {
        log::info!("nothing this server started runs; the holder exits");
        std::process::exit(0);
    }
    if arg.is_empty() {
        log::info!(
            "staying, as a process that only waits, for the {} processes this server started: \
             macOS lets them reach the local network while it runs",
            watch.len()
        );
    } else {
        log::info!("holding on for {} processes", watch.len());
    }
    let mut seen = program.as_deref().and_then(stamp);
    // A replacement is run once it has stayed the same for one look: the
    // installer may still be writing it.
    let mut settling = None;
    loop {
        std::thread::sleep(LOOK_EVERY);
        watch = look(&watch, me, &process_tree);
        if watch.is_empty() {
            log::info!("nothing this server started runs any more; the holder exits");
            std::process::exit(0);
        }
        let Some(program) = program.as_deref() else {
            continue;
        };
        let now = stamp(program);
        if now == seen || now.is_none() {
            settling = None;
            continue;
        }
        if settling != now {
            settling = now;
            continue;
        }
        log::info!("{} was replaced; holding on as the new one", program.display());
        replace_self(program, &watch);
        // It could not be run: keep holding as this build, and try the
        // next replacement.
        seen = now;
        settling = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(entries: &[(u32, u64)]) -> Watch {
        Watch(entries.iter().copied().collect())
    }

    fn seen(start: u64, below: &[(u32, u64)]) -> Option<Seen> {
        Some(Seen {
            start,
            below: below.to_vec(),
        })
    }

    #[test]
    fn a_watch_spells_itself_and_reads_back() {
        let watch = watching(&[(4321, 1700000100), (765, 1700000000)]);
        let arg = watch.to_arg();
        assert_eq!(arg, "765:1700000000,4321:1700000100");
        assert_eq!(Watch::from_arg(&arg), watch);
        assert!(Watch::from_arg("").is_empty());
    }

    #[test]
    fn a_watch_skips_what_it_cannot_read() {
        let watch = Watch::from_arg("12:34,junk,1:5,:7,8:,99:100");
        assert_eq!(watch, watching(&[(12, 34), (99, 100)]));
    }

    #[test]
    fn the_holder_waits_for_everything_descended_from_it() {
        let tree = |pid| match pid {
            10 => seen(1, &[(11, 2), (12, 3)]),
            _ => None,
        };
        let watch = look(&Watch::default(), 10, &tree);
        assert_eq!(watch, watching(&[(11, 2), (12, 3)]));
    }

    #[test]
    fn a_process_that_outlived_its_parent_is_still_waited_for() {
        // 20 left the holder's tree when its shell exited; it started 21
        // since.
        let tree = |pid| match pid {
            10 => seen(1, &[(11, 2)]),
            20 => seen(5, &[(21, 6)]),
            _ => None,
        };
        let before = watching(&[(11, 2), (20, 5)]);
        let watch = look(&before, 10, &tree);
        assert_eq!(watch, watching(&[(11, 2), (20, 5), (21, 6)]));
    }

    #[test]
    fn a_pid_that_names_another_process_now_is_let_go() {
        let tree = |pid| match pid {
            10 => seen(1, &[]),
            20 => seen(99, &[(21, 100)]),
            _ => None,
        };
        let before = watching(&[(20, 5)]);
        assert!(look(&before, 10, &tree).is_empty());
    }

    #[test]
    fn what_ended_is_let_go_and_nothing_left_is_an_empty_watch() {
        let tree = |pid| match pid {
            10 => seen(1, &[]),
            _ => None,
        };
        let before = watching(&[(11, 2), (20, 5)]);
        assert!(look(&before, 10, &tree).is_empty());
    }

    #[test]
    fn a_table_that_could_not_be_read_lets_nothing_go() {
        let before = watching(&[(11, 2), (20, 5)]);
        assert_eq!(look(&before, 10, &|_| None), before);
    }

    #[test]
    fn only_children_told_this_is_their_parent_are_helpers() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        assert!(told_parent_is(&argv(&["thinkterm-tailnet", "--parent", "4321"]), 4321));
        assert!(!told_parent_is(&argv(&["thinkterm-tailnet", "--parent", "4322"]), 4321));
        assert!(!told_parent_is(&argv(&["-zsh"]), 4321));
        assert!(!told_parent_is(&argv(&["sleep", "4321"]), 4321));
    }

    #[test]
    fn the_process_table_shows_a_child_of_this_process() {
        let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let me = std::process::id();
        let found = process_tree(me)
            .map(|seen| seen.below.iter().any(|(pid, _)| *pid == child.id()))
            .unwrap_or(false);
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "the table did not show child {}", child.id());
    }
}
