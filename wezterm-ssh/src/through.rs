//! Connections that go another way than the OS network, as an embedded
//! tailnet reaches its machines. Whoever offers such a way registers it with
//! [`set_dialer`]; only connections selecting that network use it.

use crate::config::ConfigMap;
use anyhow::{bail, Context};
use socket2::Socket;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Selects `system` (the default) or `tailnet` for every candidate address.
pub const NETWORK: &str = "thinkterm_network";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    #[default]
    System,
    Tailnet,
}

impl Network {
    pub fn from_config(config: &ConfigMap) -> anyhow::Result<Self> {
        match config.get(NETWORK).map(String::as_str) {
            None | Some("system") => Ok(Self::System),
            Some("tailnet") => {
                if config.contains_key("bindaddress")
                    || config
                        .get("proxycommand")
                        .is_some_and(|value| value != "none")
                {
                    bail!("the embedded tailnet cannot use BindAddress or ProxyCommand");
                }
                Ok(Self::Tailnet)
            }
            Some(value) => bail!("unknown SSH network {value:?}: use system or tailnet"),
        }
    }
}

/// Which IP addresses a connection may use, as AddressFamily in the SSH
/// config says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressFamily {
    Any,
    Inet,
    Inet6,
}

/// Given one of the addresses a host is tried at -- its own as the SSH
/// config resolved it, or one of its others (see crate::addresses) -- the
/// IP addresses the connection may use, the ConnectTimeout if one is set,
/// and a probe that turns true once the session is closed (so a slow dial
/// can give up): a connected socket, why there is none, or None when the
/// node cannot reach that host. None never falls back to the OS.
pub type Dialer = dyn Fn(
        &str,
        u16,
        AddressFamily,
        Option<Duration>,
        &dyn Fn() -> bool,
    ) -> Option<anyhow::Result<Socket>>
    + Send
    + Sync;

static DIALER: Mutex<Option<Arc<Dialer>>> = Mutex::new(None);

/// Registers the way sessions selecting the embedded tailnet connect.
pub fn set_dialer(dialer: Option<Arc<Dialer>>) {
    *DIALER.lock().unwrap() = dialer;
}

/// The registered dialer's connection to `host:port` for a connection that
/// is no SSH session's -- the TLS one a domain bootstrapped over SSH makes
/// to the same machine. None only when the system network was selected.
pub fn dial_registered(
    network: Network,
    host: &str,
    port: u16,
    timeout: Option<Duration>,
) -> Option<anyhow::Result<Socket>> {
    (network == Network::Tailnet).then(|| dial(host, port, AddressFamily::Any, timeout, &|| false))
}

/// The selected tailnet's connection to `host:port`. A missing node or an
/// unknown host ends the attempt on that network.
pub(crate) fn dial(
    host: &str,
    port: u16,
    family: AddressFamily,
    timeout: Option<Duration>,
    closed: &dyn Fn() -> bool,
) -> anyhow::Result<Socket> {
    let dialer = DIALER
        .lock()
        .unwrap()
        .clone()
        .context("the embedded tailnet is unavailable")?;
    dialer(host, port, family, timeout, closed)
        .with_context(|| format!("{host}:{port} is not reachable through the embedded tailnet"))?
}
