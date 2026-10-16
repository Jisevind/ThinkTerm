//! When `thinkterm-tailnet` runs and when it ends, run as the mux server runs
//! it, in a home of its own: its settings, lock, socket and log, and nothing
//! of the machine's. None of these starts a node: the tailnet is off, or
//! another process holds the node's lock.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const PROGRAM: &str = env!("CARGO_BIN_EXE_thinkterm-tailnet");

/// Short: the node's socket goes in it.
fn home() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ttn-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn command(program: &str, home: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .env("HOME", home)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Where the settings are in `home`, as the library finds them.
fn settings_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/ThinkTerm")
    } else {
        home.join(".local/share/ThinkTerm")
    }
}

/// Turns the tailnet on, with the node's lock held as another process on the
/// machine would hold it: the program waits for it, and starts no node.
fn on_and_held_elsewhere(home: &Path) -> File {
    let dir = settings_dir(home);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("tailnet.json"), r#"{"enabled":true}"#).unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("tailnet-node.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    lock
}

fn ended_within(child: &mut Child, wait: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn running(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

#[test]
fn it_ends_while_the_tailnet_is_off() {
    let home = home();
    let mut program = command(PROGRAM, home.path()).spawn().unwrap();
    let ended = ended_within(&mut program, Duration::from_secs(10));
    assert!(ended.is_some_and(|status| status.success()), "{ended:?}");
}

#[test]
fn it_stays_while_the_tailnet_is_on() {
    let home = home();
    let _held = on_and_held_elsewhere(home.path());
    let mut program = command(PROGRAM, home.path()).spawn().unwrap();
    let ended = ended_within(&mut program, Duration::from_secs(3));
    let _ = program.kill();
    let _ = program.wait();
    assert!(ended.is_none(), "it ended: {ended:?}");
}

#[test]
fn it_ends_with_the_process_that_started_it() {
    let home = home();
    let _held = on_and_held_elsewhere(home.path());
    // Started by a shell that leaves at once, as a server that exits leaves it.
    let output = command("/bin/sh", home.path())
        .arg("-c")
        .arg(r#""$0" --parent $$ </dev/null >/dev/null 2>&1 & echo $!"#)
        .arg(PROGRAM)
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let pid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let outlived = running(pid);
    if outlived {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert!(!outlived, "it outlived the process that started it");
}

#[test]
fn it_says_what_is_wrong_with_its_arguments() {
    let home = home();
    let mut program = command(PROGRAM, home.path())
        .arg("--parent")
        .spawn()
        .unwrap();
    let ended = ended_within(&mut program, Duration::from_secs(10));
    assert_eq!(ended.and_then(|status| status.code()), Some(2));
}
