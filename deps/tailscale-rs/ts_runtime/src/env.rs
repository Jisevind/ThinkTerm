use core::any::Any;
use std::sync::Arc;
use std::time::Duration;

use kameo::{
    Reply,
    actor::{ActorRef, Spawn, WeakActorRef},
    error::SendError,
    message::{Context, Message},
    reply::DelegatedReply,
};
use kameo_actors::scheduler::Scheduler;
use smol_str::SmolStr;

use crate::{
    Error, ErrorKind,
    error::ResultExt,
    registry,
    registry::{ErasedWeakRef, Forward, Registry},
    retained_bus::RetainedBus,
};

// ThinkTerm (no panic): added.
/// How long `Env::ask` waits before its first retry at an actor that is not running, and the
/// longest it waits between two.
const ASK_RETRY_FIRST: Duration = Duration::from_millis(10);
const ASK_RETRY_MAX: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub struct Env {
    pub bus: ActorRef<RetainedBus>,
    pub scheduler: ActorRef<Scheduler>,
    pub registry: ActorRef<Registry>,

    pub keys: Arc<ts_keys::NodeState>,
}

impl Env {
    pub fn new(keys: ts_keys::NodeState) -> Self {
        Self {
            bus: RetainedBus::spawn_default(),
            scheduler: Scheduler::spawn_default(),
            registry: Registry::spawn_default(),
            keys: Arc::new(keys),
        }
    }

    pub async fn subscribe<M>(&self, slf: &ActorRef<impl Message<M>>) -> Result<(), Error>
    where
        M: Clone + Send + 'static,
    {
        self.bus
            .tell(crate::retained_bus::Register(slf.clone().recipient::<M>()))
            .await
            .with_actor_info(&self.bus)?;

        Ok(())
    }

    pub async fn publish<M>(&self, msg: M) -> Result<(), Error>
    where
        M: Clone + Send + 'static,
    {
        self.bus
            .tell(crate::retained_bus::Publish::retained(msg))
            .await
            .with_actor_info(&self.bus)?;

        Ok(())
    }

    pub async fn publish_noretain<M>(&self, msg: M) -> Result<(), Error>
    where
        M: Clone + Send + 'static,
    {
        self.bus
            .tell(crate::retained_bus::Publish::unretained(msg))
            .await
            .with_actor_info(&self.bus)?;

        Ok(())
    }

    /// Register an actor in the registry under the specified `name`. If `None`, the actor is
    /// registered as the canonical actor of type `A`.
    ///
    /// If the return value is `Ok(Some(_))`, this registration replaced an existing registration
    /// for the same name. The actor ref is erased, meaning that you must try to downcast it to a
    /// `WeakActorRef<B>` for some actor type `B` if you need an `ActorRef` to send messages to.
    pub async fn register<A>(
        &self,
        name: Option<SmolStr>,
        slf: &ActorRef<A>,
    ) -> Result<Option<ErasedWeakRef>, Error>
    where
        A: kameo::Actor,
    {
        self.registry
            .ask(registry::Register::new(name, slf))
            .await
            .with_actor_info(&self.registry)
    }

    /// Look up an actor in the registry.
    pub async fn lookup_opt<A>(
        &self,
        name: Option<SmolStr>,
    ) -> Result<Option<WeakActorRef<A>>, Error>
    where
        A: kameo::Actor + Any,
    {
        let aref = self
            .registry
            .ask(registry::Lookup::<A>::new(name))
            .await
            .with_actor_info(&self.registry)?;

        Ok(aref)
    }

    /// Look up an actor in the registry.
    ///
    /// Reports an error if the actor is in the registry but dead.
    pub async fn lookup<A>(&self, name: Option<SmolStr>) -> Result<ActorRef<A>, Error>
    where
        A: kameo::Actor + Any,
    {
        let aref = self.lookup_opt::<A>(name).await?;
        self.resolve_aref(aref)
    }

    /// Ensure the given actor exists in the registry.
    ///
    /// If it doesn't, create the actor using the given function to populate its args.
    pub async fn ensure<A, F, Fut>(
        &self,
        name: Option<SmolStr>,
        args_builder: F,
    ) -> Result<(bool, ActorRef<A>), Error>
    where
        A: kameo::Actor,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = A::Args> + Send + 'static,
    {
        let (created, aref) = self
            .registry
            .ask(registry::Ensure::new(name, args_builder))
            .await?;

        Ok((created, aref))
    }

    /// Send an ask to an actor in the registry.
    ///
    /// # Parameters
    ///
    /// - `name`: the name of the actor to send the message to. If `None`, uses the canonical actor.
    /// - `msg`: the message to send.
    /// - `wait`: whether to wait for the actor to be registered if it's not present in the
    ///   registry.
    pub async fn ask<A, M>(
        &self,
        name: Option<SmolStr>,
        mut msg: M,
        wait: bool,
    ) -> Result<<A::Reply as kameo::Reply>::Ok, Error>
    where
        A: Message<M> + Any,
        M: Send + 'static,
    {
        let mk_forward = |msg| Forward::<A, _>::new(name.clone(), msg);

        // If we aren't waiting, this just looks like `return self.registry.ask(forward).await?`
        //
        // If we are, the idea is to prefer to use `registry::Forward` to do both the lookup and
        // forward steps inside the registry (atomically from the rest of the system's perspective).
        // We could do them here, but there could be a race in between (we wouldn't know if the
        // ActorRef for a given name is still alive and owning the name by the time we call it). If
        // the forward fails, we use a lookup to determine if there has been activity on the name,
        // but we throw away the result and just retry the forward for the previously discussed
        // reasons.
        //
        // ThinkTerm (no panic): an actor that stopped for good keeps its name in the registry, so
        // the lookup answers at once and the forward fails again; each retry waits a little
        // longer, up to a second, rather than spin.
        let mut pause = ASK_RETRY_FIRST;
        loop {
            let result = self.registry.ask(mk_forward(msg)).await;

            match result {
                Ok(x) => return Ok(x),
                // ThinkTerm (no panic): the registry answers with the actor's send error as its own
                // handler error.
                Err(SendError::HandlerError(SendError::ActorNotRunning(m))) if wait => {
                    msg = m;
                }
                Err(SendError::HandlerError(e)) => {
                    return Err(e).with_actor_info(&self.registry);
                }
                // The registry itself didn't take the request or answer it, e.g. while the
                // runtime shuts down.
                Err(e) => {
                    return Err(e).with_actor_info(&self.registry);
                }
            }

            tracing::trace!("in ask: actor not available, wait until register");
            self.registry
                .ask(registry::Lookup::<A>::new(name.clone()).wait(true))
                .await
                .with_actor_info(&self.registry)?;
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(ASK_RETRY_MAX);
        }
    }

    /// Send a tell to an actor in the registry.
    ///
    /// The result provides no feedback about whether the actor was running or not.
    pub async fn tell<A, M>(&self, name: Option<SmolStr>, msg: M) -> Result<(), Error>
    where
        A: Message<M> + Any,
        M: Send + 'static,
    {
        self.registry
            .tell(Forward::<A, _>::new(name, msg))
            .await
            .with_actor_info(&self.registry)
    }

    // ThinkTerm (no panic): rewritten.
    /// Forward a message to an actor through the registry.
    ///
    /// The reply comes from a task that asks on the caller's behalf, not from kameo's
    /// `Context::forward`, which in kameo 0.21 panics if the registry's mailbox is closed.
    pub fn forward<A, M>(
        &self,
        ctx: &mut Context<impl kameo::Actor, RegForwarded<A, M>>,
        name: Option<SmolStr>,
        msg: M,
    ) -> RegForwarded<A, M>
    where
        A: Message<M> + Any,
        M: Send + 'static,
    {
        let env = self.clone();
        ctx.spawn(async move { env.ask::<A, M>(name, msg, false).await })
    }

    /// Wait for a specific actor of type `A` to appear in the registry.
    pub async fn wait<A>(&self, name: Option<SmolStr>) -> Result<(), Error>
    where
        A: kameo::Actor,
    {
        self.registry
            .ask(registry::Lookup::<A>::new(name).wait(true))
            .await?;

        Ok(())
    }

    fn resolve_aref<A>(&self, aref: Option<WeakActorRef<A>>) -> Result<ActorRef<A>, Error>
    where
        A: kameo::Actor,
    {
        let aref = aref.ok_or_else(|| Error {
            kind: ErrorKind::ActorGone,
            message_ty: Some("Lookup"),
            target_actor: Some((&self.registry).into()),
        })?;

        let aref = aref.upgrade().ok_or_else(|| Error {
            kind: ErrorKind::ActorGone,
            message_ty: None,
            target_actor: Some((&aref).into()),
        })?;

        Ok(aref)
    }
}

// ThinkTerm (no panic): was a `ForwardedReply`.
pub type RegForwarded<A, M> =
    DelegatedReply<Result<<<A as Message<M>>::Reply as Reply>::Ok, Error>>;

// ThinkTerm (no panic): the tests are ours.
#[cfg(test)]
mod tests {
    use kameo::{actor::Spawn, error::Infallible, mailbox};
    use tokio::sync::Notify;

    use super::*;

    /// Answers [`Ping`], and stays in [`Hold`] until notified.
    struct Target(Arc<Notify>);

    impl kameo::Actor for Target {
        type Args = Arc<Notify>;
        type Error = Infallible;

        async fn on_start(release: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self(release))
        }
    }

    struct Hold;

    impl Message<Hold> for Target {
        type Reply = ();

        async fn handle(&mut self, _: Hold, _: &mut Context<Self, Self::Reply>) {
            self.0.notified().await;
        }
    }

    struct Ping;

    impl Message<Ping> for Target {
        type Reply = u32;

        async fn handle(&mut self, _: Ping, _: &mut Context<Self, Self::Reply>) -> u32 {
            7
        }
    }

    /// Forwards [`Ping`] through the registry, as the runtime does for the device.
    struct Front(Env);

    impl kameo::Actor for Front {
        type Args = Env;
        type Error = Infallible;

        async fn on_start(env: Self::Args, _: ActorRef<Self>) -> Result<Self, Self::Error> {
            Ok(Self(env))
        }
    }

    impl Message<Ping> for Front {
        type Reply = RegForwarded<Target, Ping>;

        async fn handle(&mut self, msg: Ping, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
            self.0.forward::<Target, Ping>(ctx, None, msg)
        }
    }

    async fn env_with(target: &ActorRef<Target>) -> Env {
        let env = Env::new(ts_keys::NodeState::generate());
        env.register(None, target).await.unwrap();
        env
    }

    /// The kind of error the front got back, if it got one from forwarding.
    async fn front_error(env: &Env) -> Option<ErrorKind> {
        match Front::spawn(env.clone()).ask(Ping).await {
            Err(SendError::HandlerError(err)) => Some(err.kind),
            _ => None,
        }
    }

    /// Whether the registry still forwards, to a second target.
    async fn registry_works(env: &Env) -> bool {
        let other = Target::spawn(Arc::default());
        env.register(Some("other".into()), &other).await.unwrap();
        let answer = env
            .ask::<Target, _>(Some("other".into()), Ping, false)
            .await;
        answer == Ok(7)
    }

    #[tokio::test]
    async fn a_forward_gets_the_actors_reply() {
        let target = Target::spawn(Arc::default());
        let env = env_with(&target).await;

        assert_eq!(env.ask::<Target, _>(None, Ping, false).await, Ok(7));
        assert_eq!(Front::spawn(env).ask(Ping).await.ok(), Some(7));
    }

    #[tokio::test]
    async fn a_full_mailbox_is_an_error_and_the_registry_carries_on() {
        let target = Target::spawn_with_mailbox(Arc::default(), mailbox::bounded(1));
        let env = env_with(&target).await;
        // One hold being handled and one waiting fill a mailbox of one.
        target.tell(Hold).await.unwrap();
        target.tell(Hold).await.unwrap();

        let err = env.ask::<Target, _>(None, Ping, false).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::MailboxFull);
        assert_eq!(front_error(&env).await, Some(ErrorKind::MailboxFull));
        assert!(registry_works(&env).await);
    }

    #[tokio::test]
    async fn a_stopped_actor_is_an_error_and_the_registry_carries_on() {
        let target = Target::spawn(Arc::default());
        let env = env_with(&target).await;
        target.stop_gracefully().await.unwrap();
        target.wait_for_shutdown().await;

        // `target` keeps the ref alive, so the registry still finds the actor, its mailbox closed.
        let err = env.ask::<Target, _>(None, Ping, false).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ActorGone);
        assert_eq!(front_error(&env).await, Some(ErrorKind::ActorGone));
        assert!(registry_works(&env).await);
    }

    #[tokio::test]
    async fn an_ask_that_waits_comes_through_once_a_new_actor_takes_the_name() {
        let target = Target::spawn(Arc::default());
        let env = env_with(&target).await;
        target.stop_gracefully().await.unwrap();
        target.wait_for_shutdown().await;

        let waiting = tokio::spawn({
            let env = env.clone();
            async move { env.ask::<Target, _>(None, Ping, true).await }
        });
        // Retrying at the stopped one, a pause at a time.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished());
        let fresh = Target::spawn(Arc::default());
        env.register(None, &fresh).await.unwrap();
        let answer = tokio::time::timeout(Duration::from_secs(3), waiting).await;
        assert_eq!(answer.unwrap().unwrap(), Ok(7));
    }

    #[tokio::test]
    async fn a_stopped_registry_is_an_error() {
        let target = Target::spawn(Arc::default());
        let env = env_with(&target).await;
        env.registry.stop_gracefully().await.unwrap();
        env.registry.wait_for_shutdown().await;

        let err = env.ask::<Target, _>(None, Ping, false).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::ActorGone);
        assert_eq!(front_error(&env).await, Some(ErrorKind::ActorGone));
    }
}
