mod uniderp;

use std::{
    collections::HashMap,
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use kameo::{
    actor::{ActorId, ActorRef, Spawn, WeakActorRef},
    error::ActorStopReason,
    message::{Context, Message},
    supervision::RestartPolicy,
};
use kameo_actors::scheduler::SetTimeout;
use ts_control::{DerpMap, DerpRegion};
use ts_derp::RegionId;
use ts_packet::PacketMut;
use ts_transport::UnderlayTransportId;

use crate::{
    Env, Error,
    dataplane::{DataplaneActor, SendUnderlay},
    disco::SendDisco,
    multiderp::uniderp::Uniderp,
    peer_tracker::{PeerDb, PeerState},
};

/// Consumes derp map updates and spawns an actor per region that runs an underlay transport.
/// Also consumes home derp indications (for this node) to notify the relevant task that it
/// should keep the transport awake even if there is no traffic.
///
/// Other than the home task (which is always kept alive to receive packets), the transport
/// tasks keep the connection alive as long as there is traffic sent or received, and for a
/// short grace period afterward. Connections are otherwise closed not in use.
pub struct Multiderp {
    region_map: HashMap<RegionId, UnderlayTransportId>,
    env: Env,
    debounce_state: DebounceState,
    peer_state: Arc<PeerDb>,
    // ThinkTerm (derp regions): added.
    /// The Uniderps this actor supervises and their regions, told apart from its other links
    /// in `on_link_died`. A region has a Uniderp from the moment it is spawned until it is gone
    /// for good, restarts included.
    regions: HashMap<ActorId, RegionId>,
    // ThinkTerm (derp regions): added.
    /// The latest DERP map, for starting a region once more. A copy: the update it came in
    /// may carry the whole netmap, which holding the update would keep.
    derp_map: DerpMap,
    // ThinkTerm (derp regions): added.
    given_up: GivenUp,
}

impl kameo::Actor for Multiderp {
    type Args = Env;
    type Error = Error;

    async fn on_start(env: Self::Args, slf: ActorRef<Self>) -> Result<Self, Self::Error> {
        env.subscribe::<Arc<ts_control::StateUpdate>>(&slf).await?;
        env.subscribe::<Arc<PeerState>>(&slf).await?;
        env.subscribe::<SendDisco>(&slf).await?;

        env.register(None, &slf).await?;

        Ok(Self {
            env,
            region_map: Default::default(),
            debounce_state: Default::default(),
            peer_state: Default::default(),
            regions: Default::default(),
            derp_map: Default::default(),
            given_up: Default::default(),
        })
    }

    async fn on_stop(
        &mut self,
        _: WeakActorRef<Self>,
        _: ActorStopReason,
    ) -> Result<(), Self::Error> {
        self.env.publish(DerpTransportMap::default()).await?;

        Ok(())
    }

    // ThinkTerm (derp regions): added.
    /// A Uniderp gone for good: kameo calls this for one only when it does not restart it.
    ///
    /// One that failed too often to be restarted is given up on its own, and its region started
    /// once more a while later. By kameo's default, a link dying for any reason but a normal
    /// stop stops this actor as well, and every other region's client with it. One that
    /// stopped normally left the DERP map; should the map have the region again by now, it is
    /// started again at once, as `ensure_region` passed it over while the Uniderp was stopping.
    async fn on_link_died(
        &mut self,
        slf: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<ControlFlow<ActorStopReason>, Self::Error> {
        let Some(region_id) = self.regions.remove(&id) else {
            // kameo's default.
            return Ok(match reason {
                ActorStopReason::Normal | ActorStopReason::SupervisorRestart => {
                    ControlFlow::Continue(())
                }
                reason => ControlFlow::Break(ActorStopReason::LinkDied {
                    id,
                    reason: Box::new(reason),
                }),
            });
        };
        let Some(slf) = slf.upgrade() else {
            return Ok(ControlFlow::Continue(()));
        };

        // The region's transport went with its Uniderp. Taken out here rather than by the
        // Uniderp as it stopped: kameo tells the supervisor before it runs the actor's
        // `on_stop`, the Uniderp restarted in its place could put its transport in first, and
        // out it went with the old one's.
        if self.region_map.remove(&region_id).is_some() {
            self.debounce_map_publish(&slf).await;
        }

        if reason.is_normal() {
            if let Some(region) = self.derp_map.get(&region_id).cloned() {
                self.ensure_region(&slf, region_id, &region).await;
            }
        } else {
            let wait = self.given_up.wait(region_id, Instant::now());
            tracing::warn!(%region_id, %reason, ?wait, "derp region given up for a while");
            let retry = SetTimeout::new(slf.downgrade(), wait, RetryRegion(region_id));
            if let Err(e) = self.env.scheduler.ask(retry).await {
                tracing::warn!(error = ?e, %region_id, "derp region given up for good");
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

impl Multiderp {
    // ThinkTerm (derp regions): added.
    /// How long a region given up waits before it is started once more, at first and at most.
    /// Each start takes a transport the dataplane keeps for good, and a child kameo keeps a
    /// record of for as long as this actor lives, so a region that keeps being given up waits
    /// longer each time (see `GivenUp`).
    const REGION_RETRY: Duration = if cfg!(test) {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(60)
    };
    const REGION_RETRY_MAX: Duration = if cfg!(test) {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(30 * 60)
    };

    #[tracing::instrument(skip_all, fields(region_id = %id))]
    async fn ensure_region(&mut self, slf: &ActorRef<Self>, id: RegionId, region: &DerpRegion) {
        // ThinkTerm (derp regions): was a lookup of the region's Uniderp in the registry, which
        // has it only once it is started: a DERP map coming meanwhile started a second one for
        // the region. Nor does a Uniderp gone for good leave the registry.
        if self.regions.values().any(|region_id| *region_id == id) {
            return;
        }

        tracing::trace!("spawn new uniderp");
        let uniderp = Uniderp::supervise(
            slf,
            uniderp::Args {
                env: self.env.clone(),
                region: region.clone(),
                region_id: id,
            },
        )
        // ThinkTerm (derp regions): a Uniderp stops normally when its region leaves the DERP
        // map; restarted, as kameo's default policy has it, it found the region gone and
        // stopped again, five times over, each time taking a transport from the dataplane.
        .restart_policy(RestartPolicy::Transient)
        .spawn()
        .await;
        // ThinkTerm (derp regions): see `on_link_died`.
        self.regions.insert(uniderp.id(), id);
    }

    /// Trailing-edge debounce for region id -> transport id map updates.
    ///
    /// Delays these messages slightly in the interest of coalescing large quantities of updates
    /// from derp map updates and uniderp spawns to avoid repetitive, partial downstream route
    /// recomputations.
    async fn debounce_map_publish(&mut self, slf: &ActorRef<Self>) {
        const DEBOUNCE_DUR: Duration = Duration::from_millis(25);

        if self.debounce_state.publish_enqueued {
            return;
        }

        self.env
            .scheduler
            .ask(SetTimeout::new(
                slf.downgrade(),
                DEBOUNCE_DUR,
                DebouncedPublish,
            ))
            .await
            .unwrap();

        self.debounce_state.publish_enqueued = true;
    }

    async fn do_map_publish(&mut self) {
        self.env
            .publish(DerpTransportMap(Arc::new(self.region_map.clone())))
            .await
            .unwrap();

        self.debounce_state.last_publish = Some(Instant::now());
    }
}

impl Message<Arc<ts_control::StateUpdate>> for Multiderp {
    type Reply = ();

    #[tracing::instrument(skip_all, name = "multiderp map update")]
    async fn handle(
        &mut self,
        msg: Arc<ts_control::StateUpdate>,
        ctx: &mut Context<Self, Self::Reply>,
    ) {
        let Some(derp_map) = &msg.derp else {
            return;
        };
        // ThinkTerm (derp regions): kept for `on_link_died` and `RetryRegion`; a region gone
        // from the map is no longer one given up.
        self.derp_map = derp_map.clone();
        self.given_up.forget_all_but(derp_map);

        let now = Instant::now();
        for (id, region) in derp_map {
            // ThinkTerm (derp regions): a region given up waits out its wait, whatever maps
            // come meanwhile; `RetryRegion` starts it.
            if self.given_up.waiting(*id, now) {
                continue;
            }
            self.ensure_region(ctx.actor_ref(), *id, region).await;
        }
    }
}

// ThinkTerm (derp regions): added.
/// Start a region given up once more, if the DERP map still has it.
struct RetryRegion(RegionId);

impl Message<RetryRegion> for Multiderp {
    type Reply = ();

    async fn handle(&mut self, RetryRegion(id): RetryRegion, ctx: &mut Context<Self, Self::Reply>) {
        let Some(region) = self.derp_map.get(&id).cloned() else {
            return;
        };
        self.ensure_region(ctx.actor_ref(), id, &region).await;
    }
}

// ThinkTerm (derp regions): added.
/// The regions given up, each with the wait it was given last and when.
#[derive(Default)]
struct GivenUp(HashMap<RegionId, (Duration, Instant)>);

impl GivenUp {
    /// The wait of region `id`, given up at `now`: twice the one before, up to the longest,
    /// when the region was given up again before it held for the longest wait since it was
    /// started once more; the first otherwise.
    fn wait(&mut self, id: RegionId, now: Instant) -> Duration {
        let wait = match self.0.get(&id) {
            Some(&(wait, at)) if now < at + wait + Multiderp::REGION_RETRY_MAX => {
                (wait * 2).min(Multiderp::REGION_RETRY_MAX)
            }
            _ => Multiderp::REGION_RETRY,
        };
        self.0.insert(id, (wait, now));
        wait
    }

    /// Whether region `id` is still waiting out its wait at `now`.
    fn waiting(&self, id: RegionId, now: Instant) -> bool {
        self.0.get(&id).is_some_and(|&(wait, at)| now < at + wait)
    }

    /// Forget the regions `derp_map` does not have.
    fn forget_all_but(&mut self, derp_map: &DerpMap) {
        self.0.retain(|id, _| derp_map.contains_key(id));
    }
}

#[derive(Clone, Debug, Default)]
pub struct DerpTransportMap(pub Arc<HashMap<RegionId, UnderlayTransportId>>);

struct SetRegionTransportId(RegionId, Option<UnderlayTransportId>);

impl Message<SetRegionTransportId> for Multiderp {
    type Reply = ();

    async fn handle(
        &mut self,
        SetRegionTransportId(region, id): SetRegionTransportId,
        _ctx: &mut Context<Self, Self::Reply>,
    ) {
        let pre = self.region_map.get(&region).copied();

        match id {
            Some(id) => {
                self.region_map.insert(region, id);
            }
            None => {
                self.region_map.remove(&region);
            }
        }

        let post = self.region_map.get(&region).copied();

        if pre != post {
            self.debounce_map_publish(_ctx.actor_ref()).await;
        }
    }
}

#[derive(Default)]
struct DebounceState {
    /// The last time a message was published.
    last_publish: Option<Instant>,

    /// Whether there is a publish timer currently enqueued.
    publish_enqueued: bool,
}

#[derive(Copy, Clone, Debug)]
struct DebouncedPublish;

impl Message<DebouncedPublish> for Multiderp {
    type Reply = ();

    async fn handle(&mut self, _: DebouncedPublish, _: &mut Context<Self, Self::Reply>) {
        self.do_map_publish().await;
        self.debounce_state.publish_enqueued = false;
    }
}

impl Message<Arc<PeerState>> for Multiderp {
    type Reply = ();

    async fn handle(&mut self, state: Arc<PeerState>, _: &mut Context<Self, Self::Reply>) {
        self.peer_state = state.peers.clone();
    }
}

impl Message<SendDisco> for Multiderp {
    type Reply = ();

    async fn handle(&mut self, send: SendDisco, _: &mut Context<Self, Self::Reply>) {
        let Some(nodekey) = send.ep.as_derp() else {
            return;
        };

        let Some((_, peer)) = self.peer_state.get(&nodekey) else {
            tracing::trace!(%nodekey, "no known peer");
            return;
        };

        let Some(derp_region) = &peer.derp_region else {
            tracing::trace!(%nodekey, "peer had no derp region");
            return;
        };

        let Some(transport_id) = self.region_map.get(derp_region) else {
            tracing::trace!(%nodekey, ?derp_region, "derp region had no transport");
            return;
        };

        let sent = self
            .env
            .ask::<DataplaneActor, _>(
                None,
                SendUnderlay {
                    endpoint: send.ep,
                    bufs: vec![PacketMut::from(send.buf.to_vec())],
                    underlay_id: *transport_id,
                },
                true,
            )
            .await
            .unwrap();

        tracing::trace!(?sent, %nodekey, ?derp_region, "disco send result");
    }
}

// ThinkTerm (derp regions): the tests are ours.
#[cfg(test)]
mod tests {
    use core::num::NonZeroU32;

    use super::*;

    #[test]
    fn a_region_given_up_again_soon_waits_longer() {
        let region = RegionId(NonZeroU32::new(1).unwrap());
        let other = RegionId(NonZeroU32::new(2).unwrap());
        let mut given_up = GivenUp::default();
        let mut now = Instant::now();

        let mut waits = vec![];
        for _ in 0..7 {
            let wait = given_up.wait(region, now);
            assert!(given_up.waiting(region, now));
            assert!(!given_up.waiting(region, now + wait));
            waits.push(wait.as_secs() / Multiderp::REGION_RETRY.as_secs());
            // Started once more after the wait, and given up again soon after.
            now += wait + Duration::from_secs(3);
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);

        // Another region starts from the first wait.
        assert_eq!(given_up.wait(other, now), Multiderp::REGION_RETRY);

        // Given up after it held for the longest wait: the first wait again.
        now += Multiderp::REGION_RETRY_MAX * 2 + Duration::from_secs(1);
        assert_eq!(given_up.wait(region, now), Multiderp::REGION_RETRY);
    }

    #[test]
    fn a_region_that_left_the_derp_map_is_forgotten_as_given_up() {
        let gone = RegionId(NonZeroU32::new(1).unwrap());
        let kept = RegionId(NonZeroU32::new(2).unwrap());
        let derp_map: DerpMap = [(
            kept,
            DerpRegion {
                info: ts_derp::RegionInfo {
                    name: "test".into(),
                    code: "t".into(),
                    no_measure_no_home: true,
                },
                servers: vec![],
            },
        )]
        .into_iter()
        .collect();
        let mut given_up = GivenUp::default();
        let now = Instant::now();
        for region in [gone, kept] {
            given_up.wait(region, now);
            given_up.wait(region, now);
        }

        given_up.forget_all_but(&derp_map);
        assert!(!given_up.waiting(gone, now));
        assert_eq!(given_up.wait(gone, now), Multiderp::REGION_RETRY);
        assert!(given_up.waiting(kept, now));
        assert_eq!(given_up.wait(kept, now), Multiderp::REGION_RETRY * 4);
    }
}
