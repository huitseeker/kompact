use super::ComponentDefinition;

use actors::Actor;
use actors::ActorPath;
use actors::ActorRef;
use actors::Dispatcher;
use actors::SystemPath;
use actors::Transport;
use bytes::Buf;
use component::Component;
use component::ComponentContext;
use component::ExecuteResult;
use component::Provide;
use lifecycle::ControlEvent;
use lifecycle::ControlPort;
use std::any::Any;
use std::net::SocketAddr;
use std::sync::Arc;

use dispatch::lookup::ActorLookup;
use futures::Async;
use futures::AsyncSink;
use futures::{self, Poll, StartSend};
use messaging::DispatchEnvelope;
use messaging::EventEnvelope;
use messaging::MsgEnvelope;
use messaging::PathResolvable;
use messaging::RegistrationEnvelope;
use net;
use net::ConnectionState;
use serialisation::helpers::serialise_to_recv_envelope;
use serialisation::Serialisable;
use spnl::frames::Frame;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Mutex;
use KompicsLogger;

mod lookup;

/// Configuration for network dispatcher
pub struct NetworkConfig {
    addr: SocketAddr,
}

/// Network-aware dispatcher for messages to remote actors.
#[derive(ComponentDefinition)]
pub struct NetworkDispatcher {
    ctx: ComponentContext<NetworkDispatcher>,
    connections: HashMap<SocketAddr, ConnectionState>,
    cfg: NetworkConfig,
    lookup: ActorLookup,
    // Fields initialized at [ControlEvent::Start]; they require ComponentContextual awareness
    net_bridge: Option<net::Bridge>,
    queue_manager: Option<QueueManager>,
}

// impl NetworkConfig
impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            addr: "127.0.0.1:8080".parse().unwrap(), // TODO remove hard-coded path
        }
    }
}

/// Wrapper around a hashmap of frame queues.
///
/// Used when waiting for connections to establish and drained when possible.
pub struct QueueManager {
    log: KompicsLogger,
    inner: HashMap<SocketAddr, VecDeque<Frame>>,
}

impl QueueManager {
    pub fn new(log: KompicsLogger) -> Self {
        QueueManager {
            log,
            inner: HashMap::new(),
        }
    }

    /// Appends the given frame onto the SocketAddr's queue
    pub fn enqueue_frame(&mut self, frame: Frame, dst: SocketAddr) {
        debug!(self.log, "Enqueuing frame");
        let queue = self.inner.entry(dst).or_insert(VecDeque::new());
        queue.push_back(frame);
    }

    /// Extracts the next queue-up frame for the SocketAddr, if one exists
    pub fn dequeue_frame(&mut self, dst: &SocketAddr) -> Option<Frame> {
        debug!(self.log, "Dequeuing frame");
        self.inner.get_mut(dst).and_then(|q| q.pop_back())
    }
}

// impl NetworkDispatcher
impl NetworkDispatcher {
    pub fn new() -> Self {
        NetworkDispatcher::default()
    }

    pub fn with_config(cfg: NetworkConfig) -> Self {
        NetworkDispatcher {
            ctx: ComponentContext::new(),
            connections: HashMap::new(),
            cfg,
            lookup: ActorLookup::new(),
            net_bridge: None,
            queue_manager: None,
        }
    }

    fn start(&mut self) {
        debug!(self.ctx.log(), "Starting self and network bridge");
        let dispatcher = {
            use actors::ActorRefFactory;
            self.actor_ref()
        };

        let bridge_logger = self.ctx().log().new(o!("owner" => "Bridge"));
        let (mut bridge, events) = net::Bridge::new(bridge_logger);
        bridge.set_dispatcher(dispatcher.clone());
        bridge.start(self.cfg.addr.clone());

        if let Some(ref ex) = bridge.executor.as_ref() {
            use futures::{Future, Stream};
            ex.spawn(
                events
                    .map(|ev| {
                        MsgEnvelope::Dispatch(DispatchEnvelope::Event(
                            EventEnvelope::Network(ev),
                        ))
                    })
                    .forward(dispatcher)
                    .then(|_| Ok(())),
            );
        } else {
            error!(
                self.ctx.log(),
                "No executor found in network bridge; network events can not be handled"
            );
        }
        let queue_manager = QueueManager::new(self.ctx().log().new(o!("owner" => "QueueManager")));
        self.net_bridge = Some(bridge);
        self.queue_manager = Some(queue_manager);
    }

    /// Forwards `msg` up to a local `dst` actor, if it exists.
    ///
    /// # Errors
    /// TODO handle unknown destination actor
    /// FIXME this fn
    fn route_local(&mut self, src: PathResolvable, dst: ActorPath, msg: Box<Serialisable>) {
        //        let actor = match dst {
        //            ActorPath::Unique(ref up) => self.lookup.get_by_uuid(up.uuid_ref()),
        //            ActorPath::Named(ref np) => self.lookup.get_by_named_path(&np.path_ref()),
        //        };
        //
        //        if let Some(actor) = actor {
        //            //  TODO err handling
        //            match msg.local() {
        //                Ok(boxed_value) => {
        //                    let src_actor_opt = match src {
        //                        ActorPath::Unique(ref up) => self.lookup.get_by_uuid(up.uuid_ref()),
        //                        ActorPath::Named(ref np) => self.lookup.get_by_named_path(&np.path_ref()),
        //                    };
        //                    if let Some(src_actor) = src_actor_opt {
        //                        actor.tell_any(boxed_value, src_actor);
        //                    } else {
        //                        panic!("Non-local ActorPath ended up in local dispatcher!");
        //                    }
        //                }
        //                Err(msg) => {
        //                    // local not implemented
        //                    let envelope = serialise_to_recv_envelope(src, dst, msg).unwrap();
        //                    actor.enqueue(envelope);
        //                }
        //            }
        //        } else {
        //            // TODO handle non-existent routes
        //            error!(self.ctx.log(), "ERR no local actor found at {:?}", dst);
        //        }
    }

    /// Routes the provided message to the destination, or queues the message until the connection
    /// is available.
    fn route_remote(&mut self, src: PathResolvable, dst: ActorPath, msg: Box<Serialisable>) {
        use actors::SystemField;
        use spnl::frames::*;

        debug!(self.ctx.log(), "Routing remote message {:?}", msg);

        // TODO serialize entire envelope into frame's payload, figure out deserialisation scheme as well
        // TODO ship over to network/tokio land

        let frame = Frame::Data(Data::with_raw_payload(0.into(), 0, "TODObytes".as_bytes()));

        let addr = SocketAddr::new(dst.address().clone(), dst.port());
        let state: &mut ConnectionState =
            self.connections.entry(addr).or_insert(ConnectionState::New);
        let next: Option<ConnectionState> = match *state {
            ConnectionState::New | ConnectionState::Closed => {
                debug!(
                    self.ctx.log(),
                    "No connection found; establishing and queuing frame"
                );
                self.queue_manager.as_mut().map(|ref mut q| q.enqueue_frame(frame, addr));

                if let Some(ref mut bridge) = self.net_bridge {
                    debug!(self.ctx.log(), "Establishing new connection to {:?}", addr);
                    bridge.connect(Transport::TCP, addr).unwrap();
                    Some(ConnectionState::Initializing)
                } else {
                    error!(self.ctx.log(), "No network bridge found; dropping message");
                    Some(ConnectionState::Closed)
                }
            }
            ConnectionState::Connected(_, ref mut tx) => {
                match tx.try_send(frame) {
                    Ok(_) => None, // Successfully relayed frame into network bridge
                    Err(e) => {
                        if e.is_full() {
                            debug!(
                                self.ctx.log(),
                                "Sender to connection is  full; buffering in Bridge"
                            );
                            let frame = e.into_inner();
                            self.queue_manager.as_mut().map(|ref mut q| q.enqueue_frame(frame, addr));
                            None
                        } else if e.is_disconnected() {
                            warn!(self.ctx.log(), "Frame receiver has been dropped; did the connection handler panic?");
                            let frame = e.into_inner();
                            self.queue_manager.as_mut().map(|ref mut q| q.enqueue_frame(frame, addr));
                            Some(ConnectionState::Closed)
                        } else {
                            // Only two error types possible
                            unreachable!();
                        }
                    }
                }
            }
            ConnectionState::Initializing => {
                debug!(self.ctx.log(), "Connection is initializing; queuing frame");
                self.queue_manager.as_mut().map(|ref mut q| q.enqueue_frame(frame, addr));
                None
            }
            _ => None,
        };

        if let Some(next) = next {
            *state = next;
        }
    }

    /// Forwards `msg` to destination described by `dst`, routing it across the network
    /// if needed.
    fn route(&mut self, src: PathResolvable, dst: ActorPath, msg: Box<Serialisable>) {
        let proto = {
            use actors::SystemField;
            let dst_sys = dst.system();
            SystemField::protocol(dst_sys)
        };
        match proto {
            Transport::LOCAL => {
                self.route_local(src, dst, msg);
            }
            Transport::TCP => {
                self.route_remote(src, dst, msg);
            }
            Transport::UDP => {
                error!(self.ctx.log(), "UDP routing not supported yet");
            }
        }
    }
}

impl Default for NetworkDispatcher {
    fn default() -> Self {
        NetworkDispatcher::with_config(NetworkConfig::default())
    }
}

impl Actor for NetworkDispatcher {
    fn receive_local(&mut self, sender: ActorRef, msg: Box<Any>) {
        debug!(self.ctx.log(), "Received LOCAL {:?} from {:?}", msg, sender);
    }
    fn receive_message(&mut self, sender: ActorPath, ser_id: u64, _buf: &mut Buf) {
        debug!(
            self.ctx.log(),
            "Received buffer with id {:?} from {:?}",
            ser_id,
            sender
        );
    }
}

impl Dispatcher for NetworkDispatcher {
    fn receive(&mut self, env: DispatchEnvelope) {
        match env {
            DispatchEnvelope::Cast(_) => {
                // Should not be here!
                error!(self.ctx.log(), "Received a cast envelope");
            }
            DispatchEnvelope::Msg { src, dst, msg } => {
                // Look up destination (local or remote), then route or err
                self.route(src, dst, msg);
            }
            DispatchEnvelope::Registration(reg) => {
                match reg {
                    RegistrationEnvelope::Register(actor, path) => {
                        self.lookup.insert(actor, path);
                    }
                    RegistrationEnvelope::Deregister(actor_path) => {
                        debug!(self.ctx.log(), "Deregistering actor at {:?}", actor_path);
                        // TODO handle
                    }
                }
            }
            DispatchEnvelope::Event(ev) => {
                // TODO
                debug!(self.ctx.log(), "Received dispacher event {:?}", ev);
            }
        }
    }

    fn system_path(&mut self) -> SystemPath {
        SystemPath::new(Transport::LOCAL, self.cfg.addr.ip(), self.cfg.addr.port())
    }
}

impl Provide<ControlPort> for NetworkDispatcher {
    fn handle(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Start => {
                self.start();
            }
            ControlEvent::Stop => info!(self.ctx.log(), "Stopping"),
            ControlEvent::Kill => info!(self.ctx.log(), "Killed"),
        }
    }
}

/// Helper for forwarding [MsgEnvelope]s to actor references
impl futures::Sink for ActorRef {
    type SinkItem = MsgEnvelope;
    type SinkError = ();

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        ActorRef::enqueue(self, item);
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::super::*;
    use super::*;

    use bytes::{Buf, BufMut};
    use component::ComponentContext;
    use component::Provide;
    use default_components::DeadletterBox;
    use lifecycle::ControlEvent;
    use lifecycle::ControlPort;
    use ports::Port;
    use runtime::KompicsConfig;
    use runtime::KompicsSystem;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    #[ignore]
    fn registration() {
        let mut cfg = KompicsConfig::new();
        cfg.system_components(DeadletterBox::new, NetworkDispatcher::default);
        let system = KompicsSystem::new(cfg);

        let component = system.create_and_register(TestComponent::new);
        // FIXME @Johan
        // let path = component.actor_path();
        // let port = component.on_definition(|this| this.rec_port.share());

        // system.trigger_i(Arc::new(String::from("local indication")), port);

        // let reference = component.actor_path();

        // reference.tell("Network me, dispatch!", &system);

        // // Sleep; allow the system to progress
        // thread::sleep(Duration::from_millis(1000));

        // match system.shutdown() {
        //     Ok(_) => println!("Successful shutdown"),
        //     Err(_) => eprintln!("Error shutting down system"),
        // }
    }

    const PING_COUNT: u64 = 10;

    #[test]
    #[ignore]
    fn local_delivery() {
        let mut cfg = KompicsConfig::new();
        cfg.system_components(DeadletterBox::new, NetworkDispatcher::default);
        let system = KompicsSystem::new(cfg);

        let ponger = system.create_and_register(PongerAct::new);
        // FIXME @Johan
        // let ponger_path = ponger.actor_path();
        // let pinger = system.create_and_register(move || PingerAct::new(ponger_path));

        // system.start(&ponger);
        // system.start(&pinger);

        // thread::sleep(Duration::from_millis(1000));

        // let pingf = system.stop_notify(&pinger);
        // let pongf = system.kill_notify(ponger);
        // pingf
        //     .await_timeout(Duration::from_millis(1000))
        //     .expect("Pinger never stopped!");
        // pongf
        //     .await_timeout(Duration::from_millis(1000))
        //     .expect("Ponger never died!");
        // pinger.on_definition(|c| {
        //     assert_eq!(c.local_count, PING_COUNT);
        // });

        // system
        //     .shutdown()
        //     .expect("Kompics didn't shut down properly");
    }

    #[derive(ComponentDefinition, Actor)]
    struct TestComponent {
        ctx: ComponentContext<TestComponent>,
    }

    impl TestComponent {
        fn new() -> Self {
            TestComponent {
                ctx: ComponentContext::new(),
            }
        }
    }

    impl Provide<ControlPort> for TestComponent {
        fn handle(&mut self, _event: <ControlPort as Port>::Request) -> () {}
    }

    #[derive(Debug, Clone)]
    struct PingMsg {
        i: u64,
    }

    #[derive(Debug, Clone)]
    struct PongMsg {
        i: u64,
    }

    struct PingPongSer;
    const PING_PONG_SER: PingPongSer = PingPongSer {};
    const PING_ID: i8 = 1;
    const PONG_ID: i8 = 2;
    impl Serialiser<PingMsg> for PingPongSer {
        fn serid(&self) -> u64 {
            42 // because why not^^
        }
        fn size_hint(&self) -> Option<usize> {
            Some(9)
        }
        fn serialise(&self, v: &PingMsg, buf: &mut BufMut) -> Result<(), SerError> {
            buf.put_i8(PING_ID);
            buf.put_u64(v.i);
            Result::Ok(())
        }
    }

    impl Serialiser<PongMsg> for PingPongSer {
        fn serid(&self) -> u64 {
            42 // because why not^^
        }
        fn size_hint(&self) -> Option<usize> {
            Some(9)
        }
        fn serialise(&self, v: &PongMsg, buf: &mut BufMut) -> Result<(), SerError> {
            buf.put_i8(PONG_ID);
            buf.put_u64(v.i);
            Result::Ok(())
        }
    }
    impl Deserialiser<PingMsg> for PingPongSer {
        fn deserialise(buf: &mut Buf) -> Result<PingMsg, SerError> {
            if buf.remaining() < 9 {
                return Err(SerError::InvalidData(format!(
                    "Serialised typed has 9bytes but only {}bytes remain in buffer.",
                    buf.remaining()
                )));
            }
            match buf.get_i8() {
                PING_ID => {
                    let i = buf.get_u64();
                    Ok(PingMsg { i })
                }
                PONG_ID => Err(SerError::InvalidType(
                    "Found PongMsg, but expected PingMsg.".into(),
                )),
                _ => Err(SerError::InvalidType(
                    "Found unkown id, but expected PingMsg.".into(),
                )),
            }
        }
    }
    impl Deserialiser<PongMsg> for PingPongSer {
        fn deserialise(buf: &mut Buf) -> Result<PongMsg, SerError> {
            if buf.remaining() < 9 {
                return Err(SerError::InvalidData(format!(
                    "Serialised typed has 9bytes but only {}bytes remain in buffer.",
                    buf.remaining()
                )));
            }
            match buf.get_i8() {
                PONG_ID => {
                    let i = buf.get_u64();
                    Ok(PongMsg { i })
                }
                PING_ID => Err(SerError::InvalidType(
                    "Found PingMsg, but expected PongMsg.".into(),
                )),
                _ => Err(SerError::InvalidType(
                    "Found unkown id, but expected PongMsg.".into(),
                )),
            }
        }
    }

    #[derive(ComponentDefinition)]
    struct PingerAct {
        ctx: ComponentContext<PingerAct>,
        target: ActorPath,
        local_count: u64,
        msg_count: u64,
    }

    impl PingerAct {
        fn new(target: ActorPath) -> PingerAct {
            PingerAct {
                ctx: ComponentContext::new(),
                target,
                local_count: 0,
                msg_count: 0,
            }
        }

        fn total_count(&self) -> u64 {
            self.local_count + self.msg_count
        }
    }

    impl Provide<ControlPort> for PingerAct {
        fn handle(&mut self, event: ControlEvent) -> () {
            match event {
                ControlEvent::Start => {
                    info!(self.ctx.log(), "Starting");
                    self.target.tell((PingMsg { i: 0 }, PING_PONG_SER), self);
                }
                _ => (),
            }
        }
    }

    impl Actor for PingerAct {
        fn receive_local(&mut self, sender: ActorRef, msg: Box<Any>) -> () {
            match msg.downcast_ref::<PongMsg>() {
                Some(ref pong) => {
                    info!(self.ctx.log(), "Got local Pong({})", pong.i);
                    self.local_count += 1;
                    if self.total_count() < PING_COUNT {
                        self.target
                            .tell((PingMsg { i: pong.i + 1 }, PING_PONG_SER), self);
                    }
                }
                None => error!(self.ctx.log(), "Got unexpected local msg from {}.", sender),
            }
        }
        fn receive_message(&mut self, sender: ActorPath, ser_id: u64, buf: &mut Buf) -> () {
            if ser_id == Serialiser::<PongMsg>::serid(&PING_PONG_SER) {
                let r: Result<PongMsg, SerError> = PingPongSer::deserialise(buf);
                match r {
                    Ok(pong) => {
                        info!(self.ctx.log(), "Got msg Pong({})", pong.i);
                        self.msg_count += 1;
                        if self.total_count() < PING_COUNT {
                            self.target
                                .tell((PingMsg { i: pong.i + 1 }, PING_PONG_SER), self);
                        }
                    }
                    Err(e) => error!(self.ctx.log(), "Error deserialising PongMsg: {:?}", e),
                }
            } else {
                error!(
                    self.ctx.log(),
                    "Got message with unexpected serialiser {} from {}",
                    ser_id,
                    sender
                );
            }
        }
    }

    #[derive(ComponentDefinition)]
    struct PongerAct {
        ctx: ComponentContext<PongerAct>,
    }

    impl PongerAct {
        fn new() -> PongerAct {
            PongerAct {
                ctx: ComponentContext::new(),
            }
        }
    }

    impl Provide<ControlPort> for PongerAct {
        fn handle(&mut self, event: ControlEvent) -> () {
            match event {
                ControlEvent::Start => {
                    info!(self.ctx.log(), "Starting");
                }
                _ => (),
            }
        }
    }

    impl Actor for PongerAct {
        fn receive_local(&mut self, sender: ActorRef, msg: Box<Any>) -> () {
            match msg.downcast_ref::<PingMsg>() {
                Some(ref ping) => {
                    info!(self.ctx.log(), "Got local Ping({})", ping.i);
                    sender.tell(Box::new(PongMsg { i: ping.i }), self);
                }
                None => error!(self.ctx.log(), "Got unexpected local msg from {}.", sender),
            }
        }
        fn receive_message(&mut self, sender: ActorPath, ser_id: u64, buf: &mut Buf) -> () {
            if ser_id == Serialiser::<PingMsg>::serid(&PING_PONG_SER) {
                let r: Result<PingMsg, SerError> = PingPongSer::deserialise(buf);
                match r {
                    Ok(ping) => {
                        info!(self.ctx.log(), "Got msg Ping({})", ping.i);
                        sender.tell((PongMsg { i: ping.i }, PING_PONG_SER), self);
                    }
                    Err(e) => error!(self.ctx.log(), "Error deserialising PingMsg: {:?}", e),
                }
            } else {
                error!(
                    self.ctx.log(),
                    "Got message with unexpected serialiser {} from {}",
                    ser_id,
                    sender
                );
            }
        }
    }
}
