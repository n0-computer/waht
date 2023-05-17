use std::collections::HashMap;
use std::time::Duration;
use std::{net::SocketAddr, time::Instant};

use crate::util::Interval;

use super::{
    proto::{Command, Request, Response},
    Key, PeerId, PeerInfo, Topic,
};

pub use actor::*;

pub const PEER_TTL: Duration = Duration::from_secs(60);
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ServerState {
    peer_id: PeerId,
    topics: HashMap<Topic, HashMap<PeerId, ProvideState>>, // signing_key: SigningKey,
    keys: HashMap<Key, KeyState>,                          // signing_key: SigningKey,
    peers: HashMap<PeerId, PeerState>,
    peer_ttl_timer: Interval
}

#[derive(Debug)]
pub struct ProvideState {
    last_provide: Instant,
}
impl Default for ProvideState {
    fn default() -> Self {
        Self {
            last_provide: Instant::now(),
        }
    }
}
impl ProvideState {
    fn provide_now(&mut self) {
        self.last_provide = Instant::now();
    }
}

#[derive(Debug, Default)]
pub struct KeyState {
    topics: HashMap<Topic, ProvideState>,
    peers: HashMap<PeerId, ProvideState>,
}

#[derive(Debug)]
pub struct PeerState {
    info: PeerInfo,
    last_seen: Instant,
}
impl From<PeerInfo> for PeerState {
    fn from(info: PeerInfo) -> Self {
        Self {
            info,
            last_seen: Instant::now(),
        }
    }
}

impl ServerState {
    pub fn new(peer_id: PeerId) -> Self {
        Self {
            // peer_id,
            topics: Default::default(),
            keys: Default::default(),
            peers: Default::default(),
            peer_id,
            peer_ttl_timer: Interval::from_now(PEER_TTL)
        }
    }


    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.peer_ttl_timer.has_elapsed(now) {
            // TODO: Use a timer map to not loop over all peers
            self.peers.retain(|_id, state| {
                now > state.last_seen + PEER_TTL
            });
            self.peer_ttl_timer.reset(now);
        }
    }

    pub fn poll_timeout(&self) -> Instant {
        self.peer_ttl_timer.next()
    }

    pub fn handle_request(
        &mut self,
        from: PeerId,
        from_addr: Option<SocketAddr>,
        mut request: Request,
    ) -> Response {
        tracing::debug!("Handle request from {}: {request:?}", hex::encode(from));
        tracing::debug!("self {:?}", self);
        let request_id = request.request_id;
        if let (Some(info), Some(from_addr)) = (request.peer_info.as_mut(), from_addr) {
            if info.use_from_addr {
                info.addrs.push(from_addr);
            }
        }
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.last_seen = Instant::now();
        }

        match request.command {
            Command::Hello(_) => {
                if let Some(peer_info) = request.peer_info {
                    let state = PeerState::from(peer_info);
                    self.peers.insert(from, state);
                };
                Response::empty(request_id)
            }
            Command::Ping => Response::empty(request_id),
            Command::ProvideTopic(cmd) => {
                let peers = self.topics.entry(cmd.topic).or_default();
                peers.entry(from).or_default().provide_now();
                Response::empty(request_id)
            }
            Command::ProvideKey(cmd) => {
                let key = self.keys.entry(cmd.key).or_default();
                key.peers.entry(from).or_default().provide_now();
                if let Some(topic) = cmd.topic {
                    key.topics.entry(topic).or_default().provide_now();
                }
                Response::empty(request_id)
            }
            Command::LookupTopic(cmd) => match self.topics.get(&cmd.topic) {
                None => Response::empty(request_id),
                Some(topic) => {
                    let peer_ids = topic.keys();
                    let peers = self.peer_infos(peer_ids).cloned().collect();
                    Response::with_peers(request_id, peers, true)
                }
            },
            Command::LookupKey(cmd) => match self.keys.get(&cmd.key) {
                Some(state) => {
                    let peer_ids = state.peers.keys();
                    let peers = self.peer_infos(peer_ids).cloned().collect();
                    Response::with_peers(request_id, peers, true)
                }
                None => Response::empty(request.request_id),
            },
            Command::Goodbye(_) => todo!(),
        }
    }

    fn peer_infos<'a>(
        &'a self,
        peer_ids: impl Iterator<Item = &'a PeerId>,
    ) -> impl Iterator<Item = &'a PeerInfo> {
        peer_ids.filter_map(|id| self.peers.get(id).map(|state| &state.info))
    }
}

mod actor {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::time::Instant;

    use flume::Receiver;
    use tokio::time::sleep_until;
    use tracing::debug;

    use super::ServerState;
    use crate::io::{ConnEvent, ConnHandle, ConnId, FromConn};
    use crate::tracker::proto::{Message, Payload};

    pub struct ServerActor {
        state: ServerState,
        in_rx: Receiver<ConnEvent>,
        conns: HashMap<ConnId, ConnHandle>,
        timeout: Pin<Box<tokio::time::Sleep>>,
    }
    impl ServerActor {
        pub fn create(state: ServerState) -> (Self, FromConn) {
            let (in_tx, in_rx) = flume::bounded(1);
            let actor = Self::new(state, in_rx);
            (actor, in_tx)
        }

        pub fn new(state: ServerState, in_rx: Receiver<ConnEvent>) -> Self {
            Self {
                timeout: Box::pin(sleep_until(state.poll_timeout().into())),
                state,
                in_rx,
                conns: Default::default(),
            }
        }
        pub async fn run(mut self) -> anyhow::Result<()> {
            loop {
                tokio::select! {
                    () = &mut self.timeout => {
                        let now = Instant::now();
                        self.state.handle_timeout(now);
                        self.timeout.as_mut().reset(self.state.poll_timeout().into());
                    }
                    event = self.in_rx.recv_async() => {
                        debug!("conn event: {event:?}");
                        let event = event?;
                        self.handle_event(event).await?;
                    }
                }
            }
        }

        async fn handle_event(&mut self, event: ConnEvent) -> anyhow::Result<()> {
            match event {
                ConnEvent::Open(handle) => {
                    self.conns.insert(handle.conn_id, handle);
                }
                ConnEvent::Packet(packet) => {
                    // Do not handle packets for unregistered conns.
                    let Some(handle) = self.conns.get_mut(&packet.conn_id) else {
                        debug!("Received packet for missing conn");
                        return Ok(())
                        // return Err(anyhow!("Received packet for invalid conn."))
                    };
                    match packet.message.payload {
                        Payload::Request(request) => {
                            let response = self.state.handle_request(
                                packet.message.from,
                                Some(handle.addr),
                                request,
                            );
                            let message = Message::response(*self.state.peer_id(), response);
                            // The error condition means the conn actor was dropped. This can happen after a
                            // conn errored and before the close event was received.
                            if let Err(_) = handle.send(message).await {
                                debug!("Conn dropped before close: {}", handle.addr);
                            }
                        }
                        Payload::Response(_res) => {
                            debug!("Conn received invalid packet: {}", handle.addr);
                            self.conns.remove(&packet.conn_id);
                        }
                        Payload::Error(err) => {
                            debug!("Conn received remote error: {:?}", err);
                            self.conns.remove(&packet.conn_id);
                        }
                    }
                }
                ConnEvent::Close(peer_id, _addr) => {
                    self.conns.remove(&peer_id);
                }
            }
            Ok(())
        }
    }
}
