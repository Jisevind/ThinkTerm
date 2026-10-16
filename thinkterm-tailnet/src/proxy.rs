//! The machine's ThinkTerm processes reach the tailnet through the one
//! holding the node, `thinkterm-tailnet`: the app, `thinkterm ssh` and the
//! mux server. A connection is asked for on
//! a socket beside the settings that only this user can open, with a line
//! naming the host; the answer is a line too, and after "ok" the socket
//! carries the connection itself. Asking needs nothing of the library: only
//! the holder, built with the `node` feature, answers.

use crate::status::Family;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
#[cfg(feature = "node")]
use {
    crate::node::{self, Shared},
    std::net::SocketAddr,
    std::os::unix::net::UnixListener,
    std::path::PathBuf,
    std::sync::Arc,
    tokio::io::{AsyncReadExt, AsyncWriteExt},
};

/// A request or an answer longer than this is not one.
const LINE_MAX: usize = 4096;

/// How long reaching the holder's socket may take: at once while it takes
/// connections, never while a stuck holder lets its backlog fill.
const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// How long past the time it was given the holder may take to answer: its
/// last look the name up can start just before that runs out.
const ANSWER_SLACK: Duration = Duration::from_secs(5);

/// The local leg of each connection gets this much buffer each way: macOS
/// gives a socket pair 8 KiB, which holds a copy over SSH far below what
/// the tailnet carries.
const SOCKET_BUFFER: usize = 512 * 1024;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Request {
    host: String,
    port: u16,
    /// Missing from the requests of a process older than the holder.
    #[serde(default)]
    family: Family,
    timeout_ms: u64,
}

/// Gives a local socket the room SOCKET_BUFFER says. Best effort: one
/// that keeps the system's default still works, only slower.
pub(crate) fn widen(socket: &impl std::os::fd::AsFd) {
    let socket = socket2::SockRef::from(socket);
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
}

/// What asking the holder of the node came to.
pub(crate) enum Asked {
    /// Nobody listens at `path`: no process here holds a node.
    Nobody,
    /// The holder went without answering, as one does whose node is on its
    /// way out (see `answer`): the node started in its place may answer.
    Gone,
    /// A connection, why there is none, or None when `host` is none of its
    /// machines or the holder kept its answer past any wait it could have.
    Answered(Option<Result<socket2::Socket>>),
}

/// What reading the holder's answer came to.
enum Answer {
    Line(String),
    /// It went without one.
    Gone,
    /// It kept it past any wait it could have.
    Late,
}

/// A connection to `host:port` through the node of the process listening
/// at `path`, which has `timeout` to make it.
pub(crate) fn ask(
    path: &Path,
    host: &str,
    port: u16,
    family: Family,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Asked {
    let Some(mut stream) = connect_within(path, CONNECT_WAIT) else {
        return Asked::Nobody;
    };
    let request = Request {
        host: host.to_string(),
        port,
        family,
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    };
    let Ok(mut line) = serde_json::to_vec(&request) else {
        return Asked::Answered(None);
    };
    line.push(b'\n');
    // Closed before it read the request: it went without answering.
    if stream.write_all(&line).is_err() {
        return Asked::Gone;
    }
    let answer = match read_answer(&mut stream, timeout, cancelled) {
        Ok(Answer::Line(answer)) => answer,
        Ok(Answer::Gone) => return Asked::Gone,
        // The selected network cannot connect.
        Ok(Answer::Late) => return Asked::Answered(None),
        Err(err) => return Asked::Answered(Some(Err(err))),
    };
    Asked::Answered(match answer.as_str() {
        "none" => None,
        "ok" => {
            widen(&stream);
            Some(
                stream
                    .set_read_timeout(None)
                    .map(|()| socket2::Socket::from(std::os::fd::OwnedFd::from(stream)))
                    .context("taking over the tailnet's connection"),
            )
        }
        answer => Some(Err(anyhow!(
            "{}",
            answer.strip_prefix("error: ").unwrap_or(answer)
        ))),
    })
}

/// A stream to the socket at `path`, if something listens there and takes
/// it within `wait`.
fn connect_within(path: &Path, wait: Duration) -> Option<UnixStream> {
    let address = socket2::SockAddr::unix(path).ok()?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).ok()?;
    socket.connect_timeout(&address, wait).ok()?;
    socket.set_nonblocking(false).ok()?;
    Some(UnixStream::from(std::os::fd::OwnedFd::from(socket)))
}

/// The holder's answer, read a byte at a time: what follows it is the
/// connection's own.
fn read_answer(
    stream: &mut UnixStream,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<Answer> {
    // macOS sets no timeout on a connection whose other end has closed: it
    // went without answering.
    if stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .is_err()
    {
        return Ok(Answer::Gone);
    }
    // The holder waits for its node to get in and for the other machine
    // within `timeout`.
    let deadline = Instant::now().checked_add(timeout.saturating_add(ANSWER_SLACK));
    let mut line = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Ok(Answer::Gone),
            Ok(_) if byte[0] == b'\n' => {
                return Ok(Answer::Line(String::from_utf8_lossy(&line).into_owned()))
            }
            Ok(_) if line.len() < LINE_MAX => line.push(byte[0]),
            Ok(_) => bail!("the tailnet's node gave no answer it could read"),
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                if cancelled() {
                    bail!("the session was closed while connecting");
                }
                // The caller turns an unanswered request into a failure.
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    log::warn!("tailnet: the process holding the node did not answer");
                    return Ok(Answer::Late);
                }
            }
            Err(_) => return Ok(Answer::Gone),
        }
    }
}

/// The socket for a node starting in this process. This process holds the
/// node (see settings::try_hold_node), so a socket already there is one a
/// holder that has gone left behind.
#[cfg(feature = "node")]
pub(crate) fn bind() -> Result<(UnixListener, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;
    let path = crate::settings::socket_path()?;
    if let Some(parent) = path.parent() {
        crate::settings::create_private_dir(parent)?;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("listening on {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("making {} private", path.display()))?;
    listener.set_nonblocking(true)?;
    Ok((listener, path))
}

/// Answers the machine's other processes for as long as the node's runtime
/// runs.
#[cfg(feature = "node")]
pub(crate) async fn serve(shared: Arc<Shared>, listener: UnixListener) {
    let listener = match tokio::net::UnixListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("tailnet: sharing the node with this machine's other processes: {err}");
            return;
        }
    };
    let user = nix::unistd::Uid::current().as_raw();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // This user's processes only, as the socket's mode says too.
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == user) {
                    tokio::spawn(answer(Arc::clone(&shared), stream));
                }
            }
            Err(err) => {
                log::warn!("tailnet: taking a connection from another process: {err}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(feature = "node")]
async fn answer(shared: Arc<Shared>, mut stream: tokio::net::UnixStream) {
    let Ok(Ok(request)) =
        tokio::time::timeout(Duration::from_secs(5), read_request(&mut stream)).await
    else {
        return;
    };
    let timeout = Duration::from_millis(request.timeout_ms);
    let started = std::time::Instant::now();
    let resolved = {
        let shared = Arc::clone(&shared);
        let host = request.host.clone();
        let family = request.family;
        tokio::task::spawn_blocking(move || {
            shared.resolve(&host, family, node::starting_wait(timeout), &|| false)
        })
        .await
    };
    // A node on its way out answers nothing, rather than that it knows no
    // such machine or cannot reach it: the asker asks the node in its place.
    let ip = match resolved {
        Ok(Some(ip)) => ip,
        Ok(None) if shared.is_going() => return,
        Ok(None) => {
            let _ = stream.write_all(b"none\n").await;
            return;
        }
        Err(_) => return,
    };
    // What is left of the time, for the connection itself.
    let left = timeout.saturating_sub(started.elapsed());
    match shared
        .tcp_connect(SocketAddr::new(ip, request.port), left)
        .await
    {
        Ok(tailnet) => {
            if stream.write_all(b"ok\n").await.is_ok() {
                widen(&stream);
                node::bridge(tailnet, stream).await;
            }
        }
        Err(_) if shared.is_going() => {}
        Err(err) => {
            let err = err.context(format!(
                "connecting to {}:{} through the tailnet",
                request.host, request.port
            ));
            let line = format!("error: {}\n", format!("{err:#}").replace('\n', " "));
            let _ = stream.write_all(line.as_bytes()).await;
        }
    }
}

#[cfg(feature = "node")]
async fn read_request(stream: &mut tokio::net::UnixStream) -> Result<Request> {
    let mut line = Vec::new();
    loop {
        match stream.read_u8().await? {
            b'\n' => break,
            _ if line.len() >= LINE_MAX => bail!("not a request"),
            byte => line.push(byte),
        }
    }
    Ok(serde_json::from_slice(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    /// A holder that answers the one request it gets with `answer`, then
    /// echoes whatever comes after. Returns the socket's path and the
    /// request it got.
    fn holder(
        answer: &'static str,
    ) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<Request>) {
        // Socket paths are short; the default temp dir on macOS is not.
        let dir = tempfile::Builder::new()
            .prefix("tt-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("tailnet.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            stream.write_all(answer.as_bytes()).unwrap();
            let mut rest = [0u8; 4];
            if stream.read_exact(&mut rest).is_ok() {
                stream.write_all(&rest).unwrap();
            }
            serde_json::from_slice(&line).unwrap()
        });
        (dir, path, served)
    }

    /// What the holder at `path` answers for `host`, port 22.
    fn answer(path: &Path, host: &str, family: Family) -> Option<Result<socket2::Socket>> {
        match ask(path, host, 22, family, Duration::from_secs(5), &|| false) {
            Asked::Answered(answered) => answered,
            Asked::Nobody => panic!("nobody listened at {}", path.display()),
            Asked::Gone => panic!("the holder at {} went without answering", path.display()),
        }
    }

    #[test]
    fn a_connection_through_another_process_carries_on_after_its_ok() {
        let (_dir, path, served) = holder("ok\n");
        let socket = answer(&path, "server-a", Family::V6)
            .expect("the holder knew it")
            .unwrap();
        let mut stream = UnixStream::from(std::os::fd::OwnedFd::from(socket));
        stream.write_all(b"ping").unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).unwrap();
        assert_eq!(&echoed, b"ping");
        assert_eq!(
            served.join().unwrap(),
            Request {
                host: "server-a".into(),
                port: 22,
                family: Family::V6,
                timeout_ms: 5000
            }
        );
    }

    #[test]
    fn a_request_without_a_family_allows_any() {
        let request: Request =
            serde_json::from_str(r#"{"host":"server-a","port":22,"timeout_ms":5000}"#).unwrap();
        assert_eq!(request.family, Family::Any);
    }

    #[test]
    fn an_unknown_host_gets_no_connection_from_the_holder() {
        let (_dir, path, served) = holder("none\n");
        assert!(answer(&path, "example.com", Family::Any).is_none());
        served.join().unwrap();
    }

    #[test]
    fn the_other_process_says_why_it_could_not_connect() {
        let (_dir, path, served) = holder("error: no answer within 5s\n");
        let err = answer(&path, "server-a", Family::Any)
            .expect("the holder knew it")
            .unwrap_err();
        assert_eq!(err.to_string(), "no answer within 5s");
        served.join().unwrap();
    }

    #[test]
    fn nobody_listening_is_told_from_a_holder_that_does_not_know_the_host() {
        let dir = tempfile::Builder::new()
            .prefix("tt-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("tailnet.sock");
        assert!(matches!(
            ask(
                &path,
                "server-a",
                22,
                Family::Any,
                Duration::from_secs(5),
                &|| false
            ),
            Asked::Nobody
        ));
    }
}
