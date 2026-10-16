//! Where the node is, as the process holding it -- `thinkterm-tailnet` --
//! last saw. It writes that to a file in the runtime folder, which is how the
//! app and `thinkterm tailnet` learn what the node is doing without asking it.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No node runs in this process.
    #[default]
    Off,
    /// Reaching the control server.
    Starting,
    /// The control server wants a person to approve this machine at `url`.
    NeedsLogin { url: String },
    Connected {
        /// This machine's name in the tailnet.
        name: String,
        address: Option<IpAddr>,
    },
    /// It stopped trying, or is waiting to try again.
    Failed { error: String },
    /// This process should hold the node, but another one on this machine
    /// does; this one takes over when that one lets go.
    HeldElsewhere,
}

/// Another machine in the tailnet.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// What a host's address can say to reach it.
    pub name: String,
    /// The name with the tailnet's domain, for when two machines share one.
    pub fqdn: String,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
    /// None while the control server has not said.
    pub online: Option<bool>,
    /// What the machine says it runs, as Tailscale names it: "macOS",
    /// "iOS", "android", "linux", "windows"... Empty when it does not say,
    /// as in a report from before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// Its Linux distribution ("ubuntu"), when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub distro: String,
    /// The model of a phone or tablet ("iPad13,4"), when it says.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
}

/// Which of a machine's addresses a connection may go to, as AddressFamily
/// in the SSH config says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    #[default]
    Any,
    V4,
    V6,
}

impl From<wezterm_ssh::AddressFamily> for Family {
    fn from(family: wezterm_ssh::AddressFamily) -> Self {
        match family {
            wezterm_ssh::AddressFamily::Any => Family::Any,
            wezterm_ssh::AddressFamily::Inet => Family::V4,
            wezterm_ssh::AddressFamily::Inet6 => Family::V6,
        }
    }
}

impl Family {
    #[cfg(feature = "node")]
    pub fn allows(self, ip: IpAddr) -> bool {
        match self {
            Family::Any => true,
            Family::V4 => ip.is_ipv4(),
            Family::V6 => ip.is_ipv6(),
        }
    }
}

impl Peer {
    /// The address a connection to it goes to.
    pub fn address(&self) -> Option<IpAddr> {
        self.address_in(Family::Any)
    }

    /// The address a connection to it goes to, of those `family` allows.
    pub(crate) fn address_in(&self, family: Family) -> Option<IpAddr> {
        let ipv4 = self.ipv4.map(IpAddr::V4);
        let ipv6 = self.ipv6.map(IpAddr::V6);
        match family {
            Family::Any => ipv4.or(ipv6),
            Family::V4 => ipv4,
            Family::V6 => ipv6,
        }
    }

    /// Whether `host`, as a host's address or SSH config spells it, names
    /// this machine.
    pub fn answers_to(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.');
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(ip)) => self.ipv4 == Some(ip),
            Ok(IpAddr::V6(ip)) => self.ipv6 == Some(ip),
            Err(_) => self.name.eq_ignore_ascii_case(host) || self.fqdn.eq_ignore_ascii_case(host),
        }
    }

    /// Whether its full name is in `domain`, a tailnet's own: one shared in
    /// from another tailnet keeps that one's.
    pub fn in_domain(&self, domain: &str) -> bool {
        self.fqdn
            .trim_end_matches('.')
            .split_once('.')
            .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(domain.trim_end_matches('.')))
    }
}

/// The status file's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The process that wrote it, to tell a report from a process that has
    /// since gone.
    pub pid: u32,
    pub status: Status,
    pub peers: Vec<Peer>,
    /// The settings' revision ([`crate::Settings::revision`]) the process
    /// had acted on when it wrote this.
    #[serde(default)]
    pub revision: u64,
}

/// Scoped to the build profile like the mux server's socket, so a debug
/// build's report never stands in for the release server's.
pub(crate) fn report_path() -> PathBuf {
    config::RUNTIME_DIR.join(config::runtime_file_name("tailnet.json"))
}

/// The last report, if a process holding a node wrote one.
pub(crate) fn read_report() -> Option<Report> {
    let text = std::fs::read_to_string(report_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The last report, if the process that wrote it is still running: one
/// left behind by a process that has since gone describes nothing. One
/// about a node in the tailnet also needs the node's socket to answer: the
/// process's number may have gone to another one since, after a crash or a
/// restart.
pub fn read_live_report() -> Option<Report> {
    read_report().filter(|report| {
        process_is_running(report.pid) && (!holds_node(&report.status) || node_answers())
    })
}

/// Whether only a process holding the node reports `status`.
fn holds_node(status: &Status) -> bool {
    matches!(
        status,
        Status::Starting | Status::NeedsLogin { .. } | Status::Connected { .. }
    )
}

/// Whether the process holding the node listens on its socket (see
/// crate::proxy), which it does for as long as it holds it.
fn node_answers() -> bool {
    #[cfg(unix)]
    {
        crate::settings::socket_path()
            .is_ok_and(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        match i32::try_from(pid) {
            Ok(pid) => matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM)),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Private to this user: a login link in it would let whoever opens it
/// first decide which tailnet this machine joins. Not waited onto the disk:
/// it is rewritten as the node changes, and means nothing after a restart.
#[cfg(all(feature = "node", any(target_os = "macos", target_os = "linux")))]
pub(crate) fn write_report(report: &Report) {
    let result = serde_json::to_vec_pretty(report)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::settings::write_private(&report_path(), &bytes, false));
    if let Err(err) = result {
        log::warn!("tailnet: writing the status file: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> Peer {
        Peer {
            name: "server-a".into(),
            fqdn: "server-a.example.ts.net".into(),
            ipv4: Some(Ipv4Addr::new(100, 64, 0, 2)),
            ipv6: None,
            online: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn a_peer_answers_to_its_names_and_addresses() {
        let peer = peer();
        assert!(peer.answers_to("server-a"));
        assert!(peer.answers_to("SERVER-A"));
        assert!(peer.answers_to("server-a.example.ts.net."));
        assert!(peer.answers_to("100.64.0.2"));
        assert!(!peer.answers_to("100.64.0.3"));
        assert!(!peer.answers_to("server-b"));
        assert!(!peer.answers_to("server-a.example.com"));
        assert!(peer.in_domain("example.ts.net"));
        assert!(peer.in_domain("EXAMPLE.ts.net."));
        assert!(!peer.in_domain("other.ts.net"));
        assert!(!peer.in_domain("ts.net"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = Report {
            pid: 42,
            status: Status::Connected {
                name: "devbox".into(),
                address: Some(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))),
            },
            peers: vec![peer()],
            revision: 7,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"state\":\"connected\""));
        assert_eq!(serde_json::from_str::<Report>(&json).unwrap(), report);
    }

    #[test]
    fn a_machine_keeps_what_it_says_it_is() {
        let phone = Peer {
            os: "iOS".into(),
            model: "iPhone15,2".into(),
            ..peer()
        };
        let json = serde_json::to_string(&phone).unwrap();
        assert_eq!(serde_json::from_str::<Peer>(&json).unwrap(), phone);
        // Written before these were kept: it says nothing about itself.
        let old = r#"{"name":"server-a","fqdn":"server-a.example.ts.net","ipv4":"100.64.0.2","ipv6":null,"online":true}"#;
        assert_eq!(serde_json::from_str::<Peer>(old).unwrap(), peer());
    }
}
