use std::{
    ops::ControlFlow,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use futures::FutureExt;
use kameo::{
    actor::{ActorId, ActorRef, Spawn, WeakActorRef},
    error::ActorStopReason,
    message::{Context, Message},
};
use smol_str::SmolStr;
use tokio::sync::{Mutex, watch};
use ts_control::DerpRegion;
use ts_dataplane::async_tokio::{FromUnderlay, Rx, ToUnderlay, Tx};
use ts_derp::RegionId;
use ts_keys::NodeKeyPair;
use ts_packet::PacketMut;
use ts_transport::{BatchRecvIter, DynEndpoint, UnderlayTransport, UnderlayTransportId};

use crate::{
    Task,
    dataplane::{DataplaneActor, NewUnderlayTransport},
    derp_latency::DerpLatencyMeasurement,
    env::Env,
    multiderp::{Multiderp, SetRegionTransportId},
    peer_tracker::{PeerDb, PeerState},
    retained_bus::GetRetained,
    task::ErasedTask,
};

#[derive(Clone)]
pub struct Args {
    pub region_id: RegionId,
    pub region: DerpRegion,
    pub env: Env,
}

/// Single-region derp client.
pub struct Uniderp {
    /// Current state to spawn a runner task.
    ///
    /// Retained here because we may need to kill and restart the runner if we get updated region
    /// info.
    runner_state: Runner,

    /// The transport id this region is responsible for.
    transport_id: UnderlayTransportId,

    /// The task runner handling the transport for this region.
    task: ActorRef<ErasedTask>,

    home_derp_tx: watch::Sender<bool>,
    // ThinkTerm (derp regions): `env` went, with the telling of the Multiderp in `on_stop`.
}

impl Uniderp {
    pub fn name(region_id: RegionId) -> SmolStr {
        smol_str::format_smolstr!("derp_region:{region_id}")
    }
}

#[kameo::messages]
impl Uniderp {
    #[message]
    pub fn transport_id(&self) -> Option<UnderlayTransportId> {
        Some(self.transport_id)
    }
}

async fn start_runner(mut runner: Runner) {
    if let Err(e) = runner.run().await {
        tracing::error!(error = %e, region_id = %runner.region_id, "running derp client");
    }
}

impl kameo::Actor for Uniderp {
    type Args = Args;
    type Error = crate::Error;

    async fn on_start(args: Self::Args, slf: ActorRef<Self>) -> Result<Self, Self::Error> {
        let (home_derp_tx, home_derp_rx) = watch::channel(false);

        let (transport_id, from_dataplane, to_dataplane) = args
            .env
            .ask::<DataplaneActor, _>(Option::<SmolStr>::None, NewUnderlayTransport, true)
            .await?;

        args.env
            .ask::<Multiderp, _>(
                None,
                SetRegionTransportId(args.region_id, Some(transport_id)),
                true,
            )
            .await?;

        let runner = Runner {
            region_id: args.region_id,
            region: args.region,
            transport_id,
            peer_db: Arc::new(RwLock::new(None)),
            home_derp_rx,
            to_dataplane,
            keys: args.env.keys.node_keys.clone(),
            from_dataplane: Arc::new(Mutex::new(from_dataplane)),
        };

        let task = Task::spawn_link(&slf, start_runner(runner.clone()).boxed()).await;

        args.env
            .subscribe::<Arc<ts_control::StateUpdate>>(&slf)
            .await?;
        args.env.subscribe::<Arc<PeerState>>(&slf).await?;
        args.env.subscribe::<DerpLatencyMeasurement>(&slf).await?;

        // ThinkTerm (derp runner): a Uniderp kameo restarts keeps its actor id, and the bus
        // hands what it retains only to ids new to it. The measurement that made the region
        // home came before the restart, and none comes again until the DERP map changes: the
        // restarted home region took itself for any other, its connection idled out, and the
        // node got nothing more through DERP.
        if let Ok(Some(measurement)) = args
            .env
            .bus
            .ask(GetRetained::<DerpLatencyMeasurement>::new())
            .await
            && let Some(home) = measurement.measurement.first()
        {
            home_derp_tx.send_replace(home.id == args.region_id);
        }

        args.env
            .register(Some(Self::name(args.region_id)), &slf)
            .await?;

        Ok(Self {
            runner_state: runner,
            transport_id,
            home_derp_tx,
            task,
        })
    }

    async fn on_stop(
        &mut self,
        _: WeakActorRef<Self>,
        _: ActorStopReason,
    ) -> Result<(), Self::Error> {
        // ThinkTerm (derp runner): the runner goes with this actor. Being linked to it is not
        // enough: kameo leaves a linked task running when a supervised actor is restarted, stops
        // normally or goes down with its supervisor. Stopped, not killed: the task tells its
        // link how it ended, which reaches the actor restarted in this one's place (same id,
        // same mailbox), and a kill would stop that one too.
        let _ = self.task.stop_gracefully().await;

        // ThinkTerm (derp runner): the queue's receiving end goes to a task that empties it
        // until the dataplane drops the other end, which it does only along with itself. Routes
        // lead here until the Multiderp has taken the transport out of its map and the routes
        // are redone, and the dataplane unwraps each send: a packet coming meanwhile panicked
        // it, and it restarted empty, carrying nothing more for the node. One parked task per
        // Uniderp stopped, as many as the transports the dataplane keeps anyway.
        let queue = self.runner_state.from_dataplane.clone();
        tokio::spawn(async move {
            let mut queue = queue.lock().await;
            while queue.recv().await.is_some() {}
        });

        // ThinkTerm (derp regions): this told the Multiderp to take the region's transport out
        // of its map, which now does so itself once the Uniderp is gone for good; see its
        // `on_link_died`.

        Ok(())
    }

    // ThinkTerm (derp runner): added.
    /// The runner ending while this actor lives means the dataplane closed its queue (see
    /// `Runner::wait_for_activity`): the region has no client, and its transport is of no more
    /// use. Failing has the actor restarted with a new one; by kameo's default, a runner that
    /// ended cleanly left this running without one. Any other link goes by the default,
    /// including a runner of the actor this one was restarted in place of (same mailbox).
    async fn on_link_died(
        &mut self,
        _: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        Ok(match reason {
            ActorStopReason::Normal | ActorStopReason::SupervisorRestart
                if id != self.task.id() =>
            {
                ControlFlow::Continue(())
            }
            reason => ControlFlow::Break(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }),
        })
    }
}

impl Message<Arc<ts_control::StateUpdate>> for Uniderp {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: Arc<ts_control::StateUpdate>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        let Some(derp) = &msg.derp else {
            return;
        };

        let Some(region) = derp.get(&self.runner_state.region_id) else {
            tracing::debug!(
                region_id = ?self.runner_state.region_id,
                "derp region disappeared from map, stopping"
            );
            ctx.stop();
            return;
        };

        if &self.runner_state.region == region {
            return;
        }

        tracing::debug!(id = %self.runner_state.region_id, "region changed, restarting task");
        self.runner_state.region = region.clone();

        self.task.unlink(ctx.actor_ref()).await;
        self.task.stop_gracefully().await.unwrap();
        self.task
            .wait_for_shutdown_with_result(|e| {
                if let Err(e) = e {
                    tracing::error!(error = ?e, "shutting down derp task on region update");
                }
            })
            .await;

        self.task = Task::spawn_link(
            ctx.actor_ref(),
            start_runner(self.runner_state.clone()).boxed(),
        )
        .await;
    }
}

impl Message<Arc<PeerState>> for Uniderp {
    type Reply = ();

    async fn handle(&mut self, msg: Arc<PeerState>, _ctx: &mut Context<Self, Self::Reply>) {
        let mut db = self.runner_state.peer_db.write().unwrap();
        *db = Some(msg.peers.clone());
    }
}

impl Message<DerpLatencyMeasurement> for Uniderp {
    type Reply = ();

    async fn handle(&mut self, msg: DerpLatencyMeasurement, _ctx: &mut Context<Self, Self::Reply>) {
        let Some(result) = msg.measurement.as_ref().first() else {
            tracing::trace!("received home derp measurement message but none was set");
            return;
        };

        self.home_derp_tx.send_if_modified(|x| {
            let new_val = result.id == self.runner_state.region_id;
            let changed = new_val != *x;
            *x = new_val;

            changed
        });
    }
}

#[derive(Clone)]
struct Runner {
    region_id: RegionId,
    region: DerpRegion,
    transport_id: UnderlayTransportId,
    home_derp_rx: watch::Receiver<bool>,
    to_dataplane: Tx<FromUnderlay>,
    from_dataplane: Arc<Mutex<Rx<ToUnderlay>>>,
    peer_db: Arc<RwLock<Option<Arc<PeerDb>>>>,
    keys: NodeKeyPair,
}

// ThinkTerm (derp runner): added.
/// What [`Runner::wait_for_activity`] found to do.
enum Activity {
    /// Connect, sending this packet first if there is one.
    Connect(Option<(DynEndpoint, Vec<PacketMut>)>),
    /// The Uniderp this runner belongs to, or the dataplane queue it serves, is gone, and the
    /// runner goes with it.
    Gone,
}

impl Runner {
    const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(10);
    // ThinkTerm (derp dial): added.
    /// How long to wait before trying the region again after it failed, at first and at most:
    /// the wait doubles with every failure in a row.
    const RETRY_FIRST: Duration = Duration::from_secs(1);
    const RETRY_MAX: Duration = Duration::from_secs(30);
    /// A connection that held this long worked: when it fails, the waits start from the first
    /// again. Counting only connections that end cleanly, a home region's, which never idles
    /// out, left every reconnect at the longest wait once a few had failed.
    const STABLE: Duration = Duration::from_secs(10);
    /// The longest a connection may take to come up: dial, TLS, upgrade and DERP handshake.
    /// None of them has a limit of its own, and a server that takes the TCP connection and then
    /// says nothing (a captive portal, a stuck proxy) held the runner for good.
    const CONNECT_TIMEOUT: Duration = if cfg!(test) {
        Duration::from_millis(300)
    } else {
        Duration::from_secs(15)
    };

    #[tracing::instrument(skip_all, fields(region_id = %self.region_id))]
    async fn run(&mut self) -> Result<(), ts_derp::Error> {
        // ThinkTerm (derp dial): a failure is waited out and the region tried again. This
        // returned it, which left the region without a client for good, or panicked with it.
        let mut retry = Self::RETRY_FIRST;
        // ThinkTerm (derp dial): the server of the region a try starts with (see `connect`).
        let mut first = 0;
        loop {
            // ThinkTerm (derp runner): the runner ends with its Uniderp.
            let Activity::Connect(pending) = self.wait_for_activity().await else {
                return Ok(());
            };
            let connect = self.connect(first, pending);
            let result = match tokio::time::timeout(Self::CONNECT_TIMEOUT, connect).await {
                Ok(Ok(transport)) => {
                    let connected = Instant::now();
                    let result = self.run_transport(transport).await;
                    if connected.elapsed() >= Self::STABLE {
                        retry = Self::RETRY_FIRST;
                    }
                    result
                }
                Ok(Err(e)) => Err(e),
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the DERP connection did not come up in time",
                )
                .into()),
            };
            match result {
                Ok(()) => retry = Self::RETRY_FIRST,
                Err(e) => {
                    tracing::warn!(error = %e, ?retry, "derp region failed, trying again");
                    first += 1;
                    if !self.wait_out(retry).await {
                        return Ok(());
                    }
                    retry = (retry * 2).min(Self::RETRY_MAX);
                }
            }
        }
    }

    // ThinkTerm (derp dial): added.
    /// Wait `wait` before the region is tried again. What is queued for it meanwhile is dropped
    /// as it comes: stale by then, and sent again by the connections it belongs to; kept, it
    /// would pile up for the whole wait. The wait ends early when the region becomes home,
    /// which is worth a try at once. False when the Uniderp or the queue went away meanwhile.
    async fn wait_out(&mut self, wait: Duration) -> bool {
        let mut from_dataplane = self.from_dataplane.lock().await;
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return true,

                changed = self.home_derp_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    if *self.home_derp_rx.borrow() {
                        return true;
                    }
                },

                queued = from_dataplane.recv() => {
                    if queued.is_none() {
                        return false;
                    }
                },
            }
        }
    }

    // ThinkTerm (derp runner): says what to do next rather than handing over a packet alone, as
    // the Uniderp, or the queue, may be gone.
    #[tracing::instrument(skip_all, level = "trace")]
    async fn wait_for_activity(&mut self) -> Activity {
        tracing::trace!("waiting for packet activity or for this to become home derp");

        let mut from_dataplane = self.from_dataplane.lock().await;

        // ThinkTerm (derp runner): the Uniderp holds the sender of `home_derp_rx`. Once it is
        // gone, `changed` fails at once, every time; taken for a wake-up, that spun this loop at
        // full speed for as long as the process lived. kameo can leave the runner of a Uniderp
        // behind (see `Uniderp::on_stop`), so the runner has to notice by itself as well. A
        // queue the dataplane closed has nothing more for this transport either; taken for
        // activity, it had the runner connect, find it closed and connect again, with no wait.
        if self.home_derp_rx.has_changed().is_err() || from_dataplane.is_closed() {
            return Activity::Gone;
        }
        while !*self.home_derp_rx.borrow_and_update() {
            tokio::select! {
                changed = self.home_derp_rx.changed() => {
                    if changed.is_err() {
                        return Activity::Gone;
                    }
                    tracing::trace!(is_home_derp = *self.home_derp_rx.borrow());
                },

                from_net = from_dataplane.recv() => {
                    tracing::trace!("received packet to send");
                    // ThinkTerm (derp runner): None is the queue closing (see above).
                    return match from_net {
                        Some(pending) => Activity::Connect(Some(pending)),
                        None => Activity::Gone,
                    };
                }
            }
        }

        Activity::Connect(None)
    }

    // ThinkTerm (derp dial): `first` added.
    #[tracing::instrument(skip_all, level = "trace")]
    async fn connect(
        &self,
        first: usize,
        pending: Option<(DynEndpoint, Vec<PacketMut>)>,
    ) -> Result<impl UnderlayTransport<Error = ts_derp::Error> + 'static, ts_derp::Error> {
        tracing::trace!("establishing derp connection");

        // ThinkTerm (derp dial): the servers from the `first`th on, round to the one before it;
        // `run` moves on by one after every failure. The dial goes down the servers only up to
        // the first that takes the TCP connection: one that then fails the TLS handshake ends
        // the try, and `CONNECT_TIMEOUT` can run out on one that says nothing at all. Starting
        // with the same server every time, the others were never tried.
        let servers = &self.region.servers;
        let first = first.checked_rem(servers.len()).unwrap_or(0);
        // Collected: rustc cannot tell the future holding the chain itself is `Send`.
        let servers: Vec<_> = servers[first..].iter().chain(&servers[..first]).collect();
        let transport = ts_derp::DefaultClient::connect(servers, &self.keys).await?;

        if let Some((info, pending)) = pending {
            tracing::trace!("sending queued packet");
            transport.send([(info, pending)]).await?;
        }

        Ok(transport)
    }

    #[tracing::instrument(skip_all, fields(transport_id = ?self.transport_id), level = "trace")]
    async fn run_transport(
        &mut self,
        transport: impl UnderlayTransport<Error = ts_derp::Error>,
    ) -> Result<(), ts_derp::Error> {
        let mut last_activity = Instant::now();
        let mut from_dataplane = self.from_dataplane.lock().await;

        loop {
            let span = tracing::trace_span!("derp_loop");

            let inactivity_timeout =
                (!*self.home_derp_rx.borrow()).then(|| last_activity + Self::INACTIVITY_TIMEOUT);

            tokio::select! {
                from_derp = transport.recv() => {
                    last_activity = Instant::now();

                    for ret in from_derp.batch_iter() {
                        let (ep, pkts) = ret?;
                        let pkts = pkts.into_iter().collect::<Vec<_>>();

                        tracing::trace!(parent: &span, ?ep, len = pkts.len(), "packets from derp server");

                        let Ok(()) = self.to_dataplane.send((self.transport_id, ep, pkts)) else {
                            tracing::error!(parent: &span, "underlay receive channel closed");
                            break;
                        };
                    }
                },

                from_net = from_dataplane.recv() => {
                    last_activity = Instant::now();

                    let Some((ep, pkts)) = from_net else {
                        tracing::warn!(parent: &span, "transport queue closed");
                        return Ok(());
                    };

                    tracing::trace!(parent: &span, ?ep, packets = pkts.len(), "packets to derp server");

                    transport.send([(ep, pkts)]).await?;
                },

                _ = option_timeout(inactivity_timeout) => {
                    if !*self.home_derp_rx.borrow_and_update() {
                        tracing::trace!(parent: &span, "timed out and not home derp, closing derp conn");
                        return Ok(());
                    }
                },

                changed = self.home_derp_rx.changed() => {
                    // ThinkTerm (derp runner): the Uniderp is gone; `wait_for_activity` ends the
                    // runner next.
                    if changed.is_err() {
                        return Ok(());
                    }
                    tracing::trace!(is_home_derp = *self.home_derp_rx.borrow());
                },
            }
        }
    }
}

async fn option_timeout(duration: Option<Instant>) {
    match duration {
        Some(dur) => tokio::time::sleep_until(dur.into()).await,
        None => core::future::pending().await,
    }
}

// ThinkTerm (derp runner, derp dial, derp regions): the tests are ours.
#[cfg(test)]
mod tests {
    use core::num::NonZeroU32;
    use std::{
        collections::HashMap,
        net::Ipv4Addr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use kameo::Reply;
    use tokio::sync::mpsc;

    use super::*;

    fn server(port: u16) -> ts_derp::ServerConnInfo {
        ts_derp::ServerConnInfo {
            hostname: "localhost".into(),
            ipv4: ts_derp::IpUsage::FixedAddr(Ipv4Addr::LOCALHOST),
            ipv6: ts_derp::IpUsage::Disable,
            tls_validation_config: ts_derp::TlsValidationConfig::CommonName {
                common_name: "localhost".into(),
            },
            https_port: port,
            stun_port: None,
            stun_only: false,
            supports_port_80: false,
        }
    }

    /// A server of a region nobody can reach: a port of this machine nothing listens on.
    async fn unreachable_server() -> ts_derp::ServerConnInfo {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        server(port)
    }

    /// A server that takes every connection and then drops it, or holds it without a word, and
    /// counts them: each is one try of the runner's.
    async fn silent_server(hold: bool) -> (ts_derp::ServerConnInfo, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let tries = Arc::new(AtomicUsize::new(0));
        let counted = tries.clone();
        tokio::spawn(async move {
            let mut held = vec![];
            while let Ok((connection, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                if hold {
                    held.push(connection);
                }
            }
        });
        (server(port), tries)
    }

    fn region(id: u32, server: ts_derp::ServerConnInfo) -> (RegionId, DerpRegion) {
        let region = DerpRegion {
            info: ts_derp::RegionInfo {
                name: format!("test {id}"),
                code: format!("t{id}"),
                no_measure_no_home: true,
            },
            servers: vec![server],
        };
        (RegionId(NonZeroU32::new(id).unwrap()), region)
    }

    /// A runner as `Uniderp::on_start` makes one, with the sender of its home-derp watch (the
    /// Uniderp's) and of its queue (the dataplane's).
    fn runner(server: ts_derp::ServerConnInfo) -> (Runner, watch::Sender<bool>, Tx<ToUnderlay>) {
        let (home_derp_tx, home_derp_rx) = watch::channel(false);
        let (to_dataplane, _) = mpsc::unbounded_channel();
        let (to_underlay, from_dataplane) = mpsc::unbounded_channel();
        let (region_id, region) = region(1, server);
        let runner = Runner {
            region_id,
            region,
            transport_id: UnderlayTransportId(1),
            home_derp_rx,
            to_dataplane,
            from_dataplane: Arc::new(Mutex::new(from_dataplane)),
            peer_db: Arc::new(RwLock::new(None)),
            keys: NodeKeyPair::random(),
        };
        (runner, home_derp_tx, to_underlay)
    }

    /// Whether `tries` reaches `count` within `within`.
    async fn tried(tries: &AtomicUsize, count: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if tries.load(Ordering::SeqCst) >= count {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_whose_uniderp_is_gone_ends() {
        let (mut runner, home_derp_tx, _queue) = runner(unreachable_server().await);
        drop(home_derp_tx);
        // 0.6.1 spun here for good.
        let ended = tokio::time::timeout(Duration::from_secs(2), runner.run()).await;
        assert!(matches!(ended, Ok(Ok(()))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiting_runner_ends_when_its_uniderp_goes() {
        let (mut runner, home_derp_tx, _queue) = runner(unreachable_server().await);
        let run = tokio::spawn(async move { runner.run().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(home_derp_tx);
        let ended = tokio::time::timeout(Duration::from_secs(2), run).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_runner_whose_queue_is_closed_ends() {
        let (mut runner, _home_derp_tx, queue) = runner(unreachable_server().await);
        drop(queue);
        let ended = tokio::time::timeout(Duration::from_secs(2), runner.run()).await;
        assert!(matches!(ended, Ok(Ok(()))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreachable_region_is_tried_again_rather_than_panicking() {
        let (mut runner, home_derp_tx, _queue) = runner(unreachable_server().await);
        // The home region: the runner connects at once, and keeps trying.
        home_derp_tx.send(true).unwrap();
        let run = tokio::spawn(async move { runner.run().await });
        // The first try fails at once, as nothing listens on the port (0.6.1 panicked there),
        // and the second comes a second later.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!run.is_finished(), "the runner stopped trying");
        drop(home_derp_tx);
        let ended = tokio::time::timeout(Duration::from_secs(3), run).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_region_that_becomes_home_is_tried_at_once() {
        let (server, tries) = silent_server(false).await;
        let (mut runner, home_derp_tx, _queue) = runner(server);
        home_derp_tx.send(true).unwrap();
        let run = tokio::spawn(async move { runner.run().await });
        // The first try fails at once (the server drops the connection); the next is due a
        // second later.
        assert!(tried(&tries, 1, Duration::from_millis(500)).await);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Chosen as home: tried at once rather than at the end of the wait.
        home_derp_tx.send(true).unwrap();
        assert!(
            tried(&tries, 2, Duration::from_millis(400)).await,
            "the runner sat out its wait"
        );
        drop(home_derp_tx);
        let ended = tokio::time::timeout(Duration::from_secs(3), run).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_that_does_not_come_up_is_given_up_in_time() {
        let (server, tries) = silent_server(true).await;
        let (mut runner, home_derp_tx, _queue) = runner(server);
        home_derp_tx.send(true).unwrap();
        let run = tokio::spawn(async move { runner.run().await });
        // Given up after CONNECT_TIMEOUT, tried again after the first wait; 0.6.1 waited on
        // the first connection for good.
        assert!(
            tried(&tries, 2, Duration::from_secs(3)).await,
            "the runner hung on its first connection"
        );
        drop(home_derp_tx);
        let ended = tokio::time::timeout(Duration::from_secs(3), run).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }

    /// The runner of a region whose first server fails as `first` does tries the next server
    /// on its next try: the dial goes no further than a server that takes the connection.
    async fn the_next_server_is_tried_after(first: ts_derp::ServerConnInfo) {
        let (next, tries) = silent_server(false).await;
        let (mut runner, home_derp_tx, _queue) = runner(first);
        runner.region.servers.push(next);
        home_derp_tx.send(true).unwrap();
        let run = tokio::spawn(async move { runner.run().await });
        // The first try fails on the first server; the next comes a second later.
        assert!(
            tried(&tries, 1, Duration::from_secs(3)).await,
            "the next server was never tried"
        );
        drop(home_derp_tx);
        let ended = tokio::time::timeout(Duration::from_secs(3), run).await;
        assert!(matches!(ended, Ok(Ok(Ok(())))), "{ended:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_next_server_is_tried_after_one_that_fails_the_handshake() {
        // It drops the connection as soon as it takes it.
        the_next_server_is_tried_after(silent_server(false).await.0).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_next_server_is_tried_after_one_that_does_not_come_up() {
        // It holds the connection without a word, until CONNECT_TIMEOUT.
        the_next_server_is_tried_after(silent_server(true).await.0).await;
    }

    #[derive(Reply)]
    struct RunnerTask(ActorRef<ErasedTask>);

    struct GetRunnerTask;

    impl Message<GetRunnerTask> for Uniderp {
        type Reply = RunnerTask;

        async fn handle(
            &mut self,
            _: GetRunnerTask,
            _: &mut Context<Self, Self::Reply>,
        ) -> RunnerTask {
            RunnerTask(self.task.clone())
        }
    }

    /// Fails the Uniderp that handles it, as a failure that is no normal stop would.
    struct Crash;

    impl Message<Crash> for Uniderp {
        type Reply = ();

        async fn handle(&mut self, _: Crash, _: &mut Context<Self, Self::Reply>) {
            panic!("a Uniderp failing on purpose");
        }
    }

    /// The Uniderp of region `id`, as the registry has it.
    async fn uniderp(env: &Env, id: u32) -> ActorRef<Uniderp> {
        let name = Uniderp::name(RegionId(NonZeroU32::new(id).unwrap()));
        for _ in 0..100 {
            if let Ok(uniderp) = env.lookup::<Uniderp>(Some(name.clone())).await {
                return uniderp;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no Uniderp for region {id}");
    }

    /// The running Uniderp of region `id` and its runner. A restart keeps the mailbox and makes
    /// a new ActorRef, which the new Uniderp registers once started; until then the registry
    /// answers with the one before, which does not take messages.
    async fn running_uniderp(env: &Env, id: u32) -> (ActorRef<Uniderp>, ActorRef<ErasedTask>) {
        for _ in 0..150 {
            let uniderp = uniderp(env, id).await;
            if let Ok(RunnerTask(task)) = uniderp.ask(GetRunnerTask).await {
                return (uniderp, task);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("region {id} has no running Uniderp");
    }

    #[derive(Reply)]
    struct RegionMap(HashMap<RegionId, UnderlayTransportId>);

    struct GetRegionMap;

    impl Message<GetRegionMap> for Multiderp {
        type Reply = RegionMap;

        async fn handle(
            &mut self,
            _: GetRegionMap,
            _: &mut Context<Self, Self::Reply>,
        ) -> RegionMap {
            RegionMap(self.region_map.clone())
        }
    }

    /// The regions the Multiderp has a transport for.
    async fn regions_with_a_transport(multiderp: &ActorRef<Multiderp>) -> Vec<u32> {
        let RegionMap(map) = multiderp.ask(GetRegionMap).await.unwrap();
        let mut regions: Vec<u32> = map.keys().map(|id| id.0.get()).collect();
        regions.sort();
        regions
    }

    /// A DERP map of regions `ids`, each with a server nobody can reach, as control sends it.
    async fn derp_map(ids: &[u32]) -> Arc<ts_control::StateUpdate> {
        let mut derp = ts_control::DerpMap::new();
        for &id in ids {
            let (id, region) = region(id, unreachable_server().await);
            derp.insert(id, region);
        }
        Arc::new(ts_control::StateUpdate {
            derp: Some(derp),
            node: None,
            peer_update: None,
            ping: None,
            packetfilter: None,
            pop_browser_url: None,
            dial_plan: None,
        })
    }

    /// The DERP clients of a node, as its runtime spawns them, with none of its regions yet.
    fn derp_clients() -> (Env, ActorRef<Multiderp>) {
        let env = Env::new(ts_keys::NodeState::generate());
        DataplaneActor::spawn(env.clone());
        let multiderp = Multiderp::spawn(env.clone());
        (env, multiderp)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_region_given_up_takes_down_neither_its_runners_nor_the_other_regions() {
        let (env, multiderp) = derp_clients();
        env.publish(derp_map(&[1, 2]).await).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let (second, _) = running_uniderp(&env, 2).await;

            // Failed once more than kameo restarts it within its window (5 times in 5 seconds).
            // Through the mailbox, which the restarted Uniderp takes over: a kill would go to
            // the one the ActorRef was made for.
            let mut runners = vec![];
            for _ in 0..6 {
                let (first, runner) = running_uniderp(&env, 1).await;
                runners.push(runner);
                first.tell(Crash).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert!(
                !uniderp(&env, 1).await.is_alive(),
                "region 1 was not given up"
            );
            assert!(multiderp.is_alive(), "Multiderp went down with region 1");
            assert!(second.is_alive(), "region 2 went down with region 1");
            for runner in &runners {
                assert!(!runner.is_alive(), "a runner outlived its Uniderp");
            }
            // Its transport went with it; the restarts left its last one in place till then.
            assert_eq!(regions_with_a_transport(&multiderp).await, [2]);

            // Started once more a while later, as the DERP map still has it.
            running_uniderp(&env, 1).await;
            assert_eq!(regions_with_a_transport(&multiderp).await, [1, 2]);
        })
        .await
        .expect("timed out");
    }

    /// Has the Multiderp take a region as given up just now, to wait this long.
    struct GivenUpFor(RegionId, Duration);

    impl Message<GivenUpFor> for Multiderp {
        type Reply = ();

        async fn handle(
            &mut self,
            GivenUpFor(id, wait): GivenUpFor,
            _: &mut Context<Self, Self::Reply>,
        ) {
            self.given_up.0.insert(id, (wait, Instant::now()));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_region_given_up_waits_out_its_wait_whatever_maps_come() {
        let (env, multiderp) = derp_clients();
        let given_up = RegionId(NonZeroU32::new(1).unwrap());
        multiderp
            .ask(GivenUpFor(given_up, Duration::from_secs(3600)))
            .await
            .unwrap();
        env.publish(derp_map(&[1, 2]).await).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), running_uniderp(&env, 2))
            .await
            .expect("timed out");
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Each Uniderp takes a transport of the dataplane as it starts, numbered from 0.
        let (next, _, _) = env
            .ask::<DataplaneActor, _>(None, NewUnderlayTransport, true)
            .await
            .unwrap();
        assert_eq!(
            next.0, 1,
            "the region given up was started before its wait was out"
        );

        // Gone from the map and back: forgotten as given up, and started at once.
        env.publish(derp_map(&[2]).await).await.unwrap();
        env.publish(derp_map(&[1, 2]).await).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), running_uniderp(&env, 1))
            .await
            .expect("timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_uniderp_whose_runner_ended_is_started_anew() {
        let (env, _multiderp) = derp_clients();
        env.publish(derp_map(&[1]).await).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let (_, ended) = running_uniderp(&env, 1).await;
            // Ended cleanly, as a runner does when the dataplane closes its queue.
            ended.stop_gracefully().await.unwrap();
            loop {
                let (_, runner) = running_uniderp(&env, 1).await;
                if runner.id() != ended.id() && runner.is_alive() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the region was left without a runner");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_region_gets_one_uniderp_however_many_maps_come_as_it_starts() {
        let (env, multiderp) = derp_clients();
        // Two maps one after the other in the Multiderp's mailbox: the second comes while the
        // Uniderps of the first are starting, which are in the registry only once started.
        // Told rather than published, for the bus may take its time with the second.
        let map = derp_map(&[1, 2]).await;
        multiderp.tell(map.clone()).await.unwrap();
        multiderp.tell(map).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            running_uniderp(&env, 1).await;
            running_uniderp(&env, 2).await;
        })
        .await
        .expect("timed out");
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Each Uniderp takes a transport of the dataplane as it starts, numbered from 0.
        let (next, _, _) = env
            .ask::<DataplaneActor, _>(None, NewUnderlayTransport, true)
            .await
            .unwrap();
        assert_eq!(next.0, 2, "a region got more than one Uniderp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_region_dropped_from_the_map_and_put_back_at_once_is_started_again() {
        let (env, multiderp) = derp_clients();
        env.publish(derp_map(&[1, 2]).await).await.unwrap();
        let without = derp_map(&[2]).await;
        let with = derp_map(&[1, 2]).await;

        tokio::time::timeout(Duration::from_secs(10), async {
            let (dropped, _) = running_uniderp(&env, 1).await;
            running_uniderp(&env, 2).await;
            // Put back while its Uniderp, told by the first to stop, is still stopping.
            env.publish(without).await.unwrap();
            env.publish(with).await.unwrap();
            loop {
                let (uniderp, _) = running_uniderp(&env, 1).await;
                if uniderp.id() != dropped.id() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!dropped.is_alive());
            assert_eq!(regions_with_a_transport(&multiderp).await, [1, 2]);
        })
        .await
        .expect("timed out");
    }

    /// Whether the Uniderp that handles it takes its region for home.
    struct IsHome;

    impl Message<IsHome> for Uniderp {
        type Reply = bool;

        async fn handle(&mut self, _: IsHome, _: &mut Context<Self, Self::Reply>) -> bool {
            *self.home_derp_tx.borrow()
        }
    }

    /// A measurement of the DERP regions with region `id` the fastest: the node's home.
    fn home_in(id: u32) -> DerpLatencyMeasurement {
        DerpLatencyMeasurement {
            measurement: Arc::new(vec![ts_netcheck::RegionResult {
                latency: Duration::from_millis(10),
                id: RegionId(NonZeroU32::new(id).unwrap()),
                latency_map_key: format!("{id}"),
                connected_remote: (Ipv4Addr::LOCALHOST, 443).into(),
            }]),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_uniderp_restarted_knows_its_region_is_home() {
        let (env, _multiderp) = derp_clients();
        env.publish(derp_map(&[1]).await).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let (uniderp, _) = running_uniderp(&env, 1).await;
            env.publish(home_in(1)).await.unwrap();
            while !uniderp.ask(IsHome).await.unwrap() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // Asked after the failure, through the same mailbox: the restarted one answers.
            uniderp.tell(Crash).await.unwrap();
            let (restarted, _) = running_uniderp(&env, 1).await;
            assert!(
                restarted.ask(IsHome).await.unwrap(),
                "the restarted Uniderp took its region for any other"
            );
        })
        .await
        .expect("timed out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_transport_of_a_uniderp_gone_still_takes_what_is_sent_to_it() {
        let (env, multiderp) = derp_clients();
        env.publish(derp_map(&[1, 2]).await).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            let (gone, _) = running_uniderp(&env, 1).await;
            running_uniderp(&env, 2).await;
            let RegionMap(map) = multiderp.ask(GetRegionMap).await.unwrap();
            let transport = map[&RegionId(NonZeroU32::new(1).unwrap())];

            // Its region leaves the DERP map; whatever it held is let go.
            env.publish(derp_map(&[2]).await).await.unwrap();
            while gone.is_alive() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;

            // As the dataplane sends to it while routes still lead there: it unwraps the send.
            let taken = env
                .ask::<DataplaneActor, _>(
                    None,
                    crate::dataplane::SendUnderlay {
                        underlay_id: transport,
                        endpoint: DynEndpoint::derp(NodeKeyPair::random().public),
                        bufs: vec![],
                    },
                    true,
                )
                .await
                .unwrap();
            assert!(
                taken,
                "the transport's queue is closed: a send panics the dataplane"
            );
        })
        .await
        .expect("timed out");
    }
}
