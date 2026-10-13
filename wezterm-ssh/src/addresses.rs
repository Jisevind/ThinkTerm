//! Reaching a host at one of several addresses.
//!
//! A host can be listed with addresses besides its own, say a LAN name, a
//! Tailscale name and a public address with a forwarded port. They are
//! tried in order, each a moment after the one before (or at once when
//! that one has failed), and the first to accept a connection is used.
//! Only the TCP connection is raced: the ssh handshake, the host key check
//! -- made against the host's own name and port, whichever address carried
//! the connection -- and authentication then happen once, on that socket.
//! An other address whose server holds a different key than the one on
//! record for the host is some other machine, and is passed over for the
//! rest. Whichever address got a session through last is tried first next
//! time, for a while.

use crate::config::ConfigMap;
use anyhow::{bail, Context};
use socket2::{Domain, Socket, Type};
use std::collections::VecDeque;
use std::net::ToSocketAddrs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The config key that lists a host's other addresses, separated by
/// spaces: each `host` or `host:port` (`[v6]:port` for an IPv6 address
/// with a port). Without a port the host's port applies.
pub const OTHER_ADDRESSES: &str = "thinkterm_other_addresses";

/// How long an address has before the next one is tried as well.
const STAGGER: Duration = Duration::from_millis(300);

/// How many hosts' last good address are remembered, and for how long.
const REMEMBERED: usize = 64;
const REMEMBERED_FOR: Duration = Duration::from_secs(60 * 60);

/// The address that last got a session through, per host and port, and
/// when.
static LAST_REACHED: Mutex<VecDeque<(String, (String, u16), Instant)>> =
    Mutex::new(VecDeque::new());

/// `host`, `host:port` or `[v6]:port`.
pub fn parse_address(text: &str, default_port: u16) -> Option<(String, u16)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        return match rest.strip_prefix(':') {
            Some(port) => Some((host.to_string(), port.parse().ok()?)),
            None if rest.is_empty() => Some((host.to_string(), default_port)),
            None => None,
        };
    }
    match text.split_once(':') {
        // One colon: a port. More than one: an IPv6 address without one.
        Some((host, port)) if !port.contains(':') => Some((host.to_string(), port.parse().ok()?)),
        _ => Some((text.to_string(), default_port)),
    }
}

/// The addresses to try for `hostname:port`, in order: the one that last
/// got through first, then the host's own, then the others it lists.
pub fn candidates(config: &ConfigMap, hostname: &str, port: u16) -> Vec<(String, u16)> {
    let mut list = vec![(hostname.to_string(), port)];
    if let Some(others) = config.get(OTHER_ADDRESSES) {
        for address in others.split_whitespace() {
            if let Some(address) = parse_address(address, port) {
                if !list.contains(&address) {
                    list.push(address);
                }
            }
        }
    }
    if list.len() > 1 {
        if let Some(last) = last_reached(hostname, port) {
            if let Some(at) = list.iter().position(|address| *address == last) {
                let last = list.remove(at);
                list.insert(0, last);
            }
        }
    }
    list
}

fn last_reached(hostname: &str, port: u16) -> Option<(String, u16)> {
    let key = format!("{hostname}:{port}");
    let mut last = LAST_REACHED.lock().unwrap();
    last.retain(|(_, _, at)| at.elapsed() < REMEMBERED_FOR);
    last.iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, address, _)| address.clone())
}

pub fn remember_reached(hostname: &str, port: u16, address: &(String, u16)) {
    let key = format!("{hostname}:{port}");
    let mut last = LAST_REACHED.lock().unwrap();
    last.retain(|(k, _, at)| *k != key && at.elapsed() < REMEMBERED_FOR);
    if last.len() >= REMEMBERED {
        last.pop_front();
    }
    last.push_back((key, address.clone(), Instant::now()));
}

/// None of the host's addresses works now: the next attempt starts over
/// from the host's own.
pub fn forget_reached(hostname: &str, port: u16) {
    let key = format!("{hostname}:{port}");
    LAST_REACHED.lock().unwrap().retain(|(k, _, _)| *k != key);
}

/// What opening one TCP connection needs from the config, so that it can
/// be done off the session's thread.
pub struct Dialer {
    family: Option<String>,
    bind: Option<String>,
    timeout: Option<Duration>,
}

impl Dialer {
    pub fn from_config(config: &ConfigMap) -> Self {
        Self {
            family: config.get("addressfamily").cloned(),
            bind: config.get("bindaddress").cloned(),
            timeout: config
                .get("connecttimeout")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs),
        }
    }

    fn family_allows(&self, addr: &std::net::SocketAddr) -> bool {
        match self.family.as_deref() {
            Some("inet") => addr.is_ipv4(),
            Some("inet6") => addr.is_ipv6(),
            None | Some("any") | Some(_) => true,
        }
    }

    pub fn dial(&self, host: &str, port: u16) -> anyhow::Result<Socket> {
        self.dial_with(host, port, false, |_| Ok(()))
    }

    /// `dial`, logging what it resolves and binds when `verbose`, and
    /// handing the socket to `before_connect` before the connect that can
    /// block: a session watches it there, to cut it short on shutdown.
    pub fn dial_with(
        &self,
        host: &str,
        port: u16,
        verbose: bool,
        before_connect: impl FnOnce(&Socket) -> anyhow::Result<()>,
    ) -> anyhow::Result<Socket> {
        let addr = (host, port)
            .to_socket_addrs()?
            .find(|addr| self.family_allows(addr))
            .with_context(|| format!("resolving address for {host}"))?;
        if verbose {
            log::info!("resolved {host}:{port} -> {addr:?}");
        }
        let sock = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
        before_connect(&sock)?;
        if let Some(bind_addr) = &self.bind {
            let bind_addr = (bind_addr.as_str(), 0)
                .to_socket_addrs()?
                .find(|addr| self.family_allows(addr))
                .with_context(|| format!("resolving bind address {bind_addr:?}"))?;
            if verbose {
                log::info!("binding to {bind_addr:?}");
            }
            sock.bind(&bind_addr.into())
                .with_context(|| format!("binding to {bind_addr:?}"))?;
        }
        match self.timeout {
            Some(timeout) => {
                sock.connect_timeout(&addr.into(), timeout)
                    .with_context(|| {
                        format!("Connecting to {host}:{port} ({addr:?}) within {timeout:?}")
                    })?;
                sock.set_nonblocking(false).with_context(|| {
                    format!("restoring blocking mode after connect to {host}:{port}")
                })?;
            }
            None => sock
                .connect(&addr.into())
                .with_context(|| format!("Connecting to {host}:{port} ({addr:?})"))?,
        }
        Ok(sock)
    }
}

/// Connect to the first of `candidates` that answers, starting each a
/// moment after the one before, or as soon as every one started so far has
/// failed. Gives back the socket and which candidate it reached. `stop`
/// is asked between waits, to give up when the session is shutting down.
pub fn race(
    candidates: &[(String, u16)],
    dialer: Dialer,
    stop: impl Fn() -> anyhow::Result<()>,
) -> anyhow::Result<(Socket, usize)> {
    let dialer = std::sync::Arc::new(dialer);
    let (tx, rx) = std::sync::mpsc::channel();
    let start = Instant::now();
    let mut started = 0;
    let mut pending = 0;
    let mut failures = vec![];
    loop {
        let due = start + STAGGER * started as u32;
        if started < candidates.len() && (pending == 0 || Instant::now() >= due) {
            let (host, port) = candidates[started].clone();
            let index = started;
            let tx = tx.clone();
            let dialer = std::sync::Arc::clone(&dialer);
            std::thread::spawn(move || {
                // Nobody listening any more just means another won.
                let _ = tx.send((index, dialer.dial(&host, port)));
            });
            started += 1;
            pending += 1;
            continue;
        }
        if pending == 0 {
            break;
        }
        let wait = match started < candidates.len() {
            true => due.saturating_duration_since(Instant::now()),
            false => Duration::from_millis(100),
        };
        match rx.recv_timeout(wait) {
            Ok((index, Ok(sock))) => return Ok((sock, index)),
            Ok((index, Err(err))) => {
                pending -= 1;
                let (host, port) = &candidates[index];
                failures.push(format!("{host}:{port}: {err:#}"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        stop()?;
    }
    bail!(
        "none of the host's addresses answered: {}",
        failures.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_take_a_port_or_the_hosts_own() {
        assert_eq!(
            parse_address("myhost.local", 22),
            Some(("myhost.local".into(), 22))
        );
        assert_eq!(
            parse_address("203.0.113.7:2222", 22),
            Some(("203.0.113.7".into(), 2222))
        );
        assert_eq!(
            parse_address("[2001:db8::1]:2200", 22),
            Some(("2001:db8::1".into(), 2200))
        );
        assert_eq!(
            parse_address("[2001:db8::1]", 22),
            Some(("2001:db8::1".into(), 22))
        );
        assert_eq!(
            parse_address("2001:db8::1", 22),
            Some(("2001:db8::1".into(), 22))
        );
        assert_eq!(parse_address("myhost:port", 22), None);
        assert_eq!(parse_address("  ", 22), None);
    }

    #[test]
    fn the_address_that_got_through_last_goes_first() {
        let mut config = ConfigMap::new();
        config.insert(
            OTHER_ADDRESSES.to_string(),
            "server.lan  server.tailnet:2200 server.lan".to_string(),
        );
        let list = candidates(&config, "server", 22);
        assert_eq!(
            list,
            vec![
                ("server".to_string(), 22),
                ("server.lan".to_string(), 22),
                ("server.tailnet".to_string(), 2200),
            ]
        );
        remember_reached("server", 22, &("server.tailnet".to_string(), 2200));
        assert_eq!(
            candidates(&config, "server", 22)[0],
            ("server.tailnet".to_string(), 2200)
        );
        // When none of them works, the next attempt starts from the host's own.
        forget_reached("server", 22);
        assert_eq!(
            candidates(&config, "server", 22)[0],
            ("server".to_string(), 22)
        );
        // Without other addresses there is nothing to reorder.
        assert_eq!(
            candidates(&ConfigMap::new(), "server", 22),
            vec![("server".to_string(), 22)]
        );
    }

    #[test]
    fn an_address_that_does_not_answer_gives_way_to_one_that_does() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Bound and closed again: nothing answers there.
        let refused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let refused_port = refused.local_addr().unwrap().port();
        drop(refused);

        let mut config = ConfigMap::new();
        config.insert("connecttimeout".to_string(), "5".to_string());
        let candidates = vec![
            ("127.0.0.1".to_string(), refused_port),
            ("127.0.0.1".to_string(), port),
        ];
        let started = Instant::now();
        let (_sock, index) = race(&candidates, Dialer::from_config(&config), || Ok(())).unwrap();
        assert_eq!(index, 1);
        // The refusal starts the next one at once, without the stagger.
        assert!(started.elapsed() < STAGGER, "took {:?}", started.elapsed());

        let all_refused = vec![("127.0.0.1".to_string(), refused_port)];
        let err = race(&all_refused, Dialer::from_config(&config), || Ok(())).unwrap_err();
        assert!(format!("{err:#}").contains("none of the host's addresses answered"));
    }
}
