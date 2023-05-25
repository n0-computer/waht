use std::collections::HashMap;
use std::time::Duration;
use std::{net::SocketAddr, time::Instant};

use crate::util::time::{ExpirationMap, Interval};

use super::{
    proto::{Command, Request, Response},
    Key, PeerId, PeerInfo, Topic,
};

pub use actor::*;

/// Default TTL for peers until they have to renew to the tracker
pub const PEER_TTL: Duration = Duration::from_secs(60 * 5);

/// Interval in which TTLs are checked
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct TrackerConfig {
    peer_ttl: Duration,
}
impl Default for TrackerConfig {
    fn default() -> Self {
        Self {
            peer_ttl: Duration::from_secs(60 * 2),
        }
    }
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

/// Tracker server state.
///
/// Maintains tables of peers and provided keys/values. Tracks TTLs.
#[derive(Debug)]
pub struct ServerState {
    // config: TrackerConfig,
    peer_id: PeerId,
    keys: HashMap<Key, KeyState>,
    peers: HashMap<PeerId, PeerState>,
    peers_ttl: ExpirationMap<PeerId>,
    // topics: HashMap<Topic, HashMap<PeerId, ProvideState>>,
    // topics_ttl: ExpirationMap<TopicId>,
    tick_interval: Interval,
}

impl ServerState {
    pub fn new(peer_id: PeerId) -> Self {
        Self::with_config(peer_id, Default::default())
    }

    pub fn with_config(peer_id: PeerId, config: TrackerConfig) -> Self {
        let now = Instant::now();
        Self {
            peer_id,
            keys: Default::default(),
            peers: Default::default(),
            peers_ttl: ExpirationMap::new(config.peer_ttl),
            tick_interval: Interval::new(now, TICK_INTERVAL),
            // topics: Default::default(),
            // config,
        }
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        for (peer_id, _) in self.peers_ttl.drain_expired(now) {
            self.peers.remove(&peer_id);
        }
    }

    pub fn poll_timeout(&self) -> Instant {
        self.tick_interval.next_tick()
    }

    pub fn handle_request(
        &mut self,
        from: PeerId,
        mut request: Request,
        from_addr: Option<SocketAddr>,
    ) -> Response {
        let now = Instant::now();
        tracing::debug!("handle request from {}: {request:?}", hex::encode(from));
        let request_id = request.request_id;
        if let (Some(info), Some(from_addr)) = (request.peer_info.as_mut(), from_addr) {
            if info.use_from_addr {
                info.addrs.push(from_addr);
            }
        }
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.last_seen = now;
            self.peers_ttl.renew(from, now);
        }

        match request.command {
            Command::Hello(_) => {
                if let Some(peer_info) = request.peer_info {
                    let state = PeerState::from(peer_info);
                    self.peers.insert(from, state);
                    self.peers_ttl.renew(from, now);
                };
                Response::empty(request_id)
            }
            Command::Ping => Response::empty(request_id),
            Command::LookupPeer(cmd) => match self.peers.get(&cmd.peer_id) {
                None => Response::empty(request_id),
                Some(peer) => Response::with_peers(request_id, vec![peer.info.clone()], true),
            },
            Command::ProvideKey(cmd) => {
                let key = self.keys.entry(cmd.key).or_default();
                key.peers.entry(from).or_default().provide_now();
                if let Some(topic) = cmd.topic {
                    key.topics.entry(topic).or_default().provide_now();
                }
                Response::empty(request_id)
            }
            Command::LookupKey(cmd) => match self.keys.get(&cmd.key) {
                Some(state) => {
                    let peer_ids = state.peers.keys();
                    let peers = self.peer_infos(peer_ids).cloned().collect();
                    Response::with_peers(request_id, peers, true)
                }
                None => Response::empty(request.request_id),
            },
            // Command::LookupTopic(cmd) => match self.topics.get(&cmd.topic) {
            //     None => Response::empty(request_id),
            //     Some(topic) => {
            //         let peer_ids = topic.keys();
            //         let peers = self.peer_infos(peer_ids).cloned().collect();
            //         Response::with_peers(request_id, peers, true)
            //     }
            // },
            // Command::ProvideTopic(cmd) => {
            //     let peers = self.topics.entry(cmd.topic).or_default();
            //     peers.entry(from).or_default().provide_now();
            //     Response::empty(request_id)
            // }
            // Command::Goodbye(_) => todo!(),
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

    use anyhow::Context;
    use tokio::sync::mpsc::{self, Receiver, Sender};
    use tokio::time::sleep_until;
    use tracing::debug;

    use super::ServerState;
    use crate::io::{ConnEvent, ConnHandle, ConnId, FROM_CONN_CHANNEL_CAP};
    use crate::tracker::proto::{Message, Payload};

    pub struct ServerActor {
        state: ServerState,
        in_rx: Receiver<ConnEvent>,
        conns: HashMap<ConnId, ConnHandle>,
        timeout: Pin<Box<tokio::time::Sleep>>,
    }
    impl ServerActor {
        pub fn create(state: ServerState) -> (Self, Sender<ConnEvent>) {
            let (in_tx, in_rx) = mpsc::channel(FROM_CONN_CHANNEL_CAP);
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
                    event = self.in_rx.recv() => {
                        let event = event.context("conn actor dropped")?;
                        self.handle_event(event).await?;
                    }
                }
            }
        }

        async fn handle_event(&mut self, event: ConnEvent) -> anyhow::Result<()> {
            match event {
                ConnEvent::Open(conn_id, handle) => {
                    debug!(conn_id, "conn open from {}", handle.remote_address());
                    self.conns.insert(conn_id, handle);
                }
                ConnEvent::Packet(conn_id, message) => {
                    // Do not handle packets for unregistered conns.
                    let Some(handle) = self.conns.get_mut(&conn_id) else {
                        debug!(conn_id, "conn not open, drop packet");
                        return Ok(())
                        // return Err(anyhow!("Received packet for invalid conn."))
                    };
                    debug!(conn_id, "conn recv packet {message:?}");
                    match message.payload {
                        Payload::Request(request) => {
                            let response = self.state.handle_request(
                                message.from,
                                request,
                                Some(handle.remote_address()),
                            );
                            let message = Message::response(*self.state.peer_id(), response);
                            // The error condition means the conn actor was dropped. This can happen after a
                            // conn errored and before the close event was received.
                            if let Err(_) = handle.send(message).await {
                                debug!(
                                    conn_id,
                                    "conn dropped before close: {}",
                                    handle.remote_address()
                                );
                            }
                        }
                        Payload::Response(_res) => {
                            debug!(conn_id, "conn recv response: invalid, drop conn");
                            self.conns.remove(&conn_id);
                        }
                        Payload::Error(err) => {
                            debug!("conn recv remote error: {err:?}");
                            self.conns.remove(&conn_id);
                        }
                    }
                }
                ConnEvent::Close(conn_id) => {
                    self.conns.remove(&conn_id);
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use crate::tracker::proto;

    use super::*;

    #[test]
    fn requests_peer_ttl() {
        let config = TrackerConfig {
            peer_ttl: Duration::from_secs(1),
        };

        let p1_id = [1u8; 32];
        let p1_info = PeerInfo::with_addr("127.0.0.1:1000".parse().unwrap());

        let p2_id = [2u8; 32];
        let p2_info = PeerInfo::with_addr("127.0.0.1:2000".parse().unwrap());

        let now = Instant::now();
        let mut state = ServerState::with_config(Default::default(), config);
        let req = Request {
            command: Command::Hello(proto::Hello {}),
            peer_info: Some(p1_info.clone()),
            request_id: 1,
        };
        state.handle_request(p1_id, req, None);

        let req = Request {
            command: Command::LookupPeer(proto::LookupPeer {
                peer_id: p1_id,
                topic: None,
            }),
            peer_info: Some(p2_info.clone()),
            request_id: 1,
        };
        let res = state.handle_request(p2_id, req, None);
        assert_eq!(res.peers.as_ref().map(|x| x.len()), Some(1));
        assert_eq!(res.peers.unwrap()[0].addrs, p1_info.addrs);

        state.handle_timeout(now + Duration::from_millis(1001));

        let req = Request {
            command: Command::LookupPeer(proto::LookupPeer {
                peer_id: p1_id,
                topic: None,
            }),
            peer_info: Some(p2_info),
            request_id: 2,
        };
        let res = state.handle_request(p2_id, req, None);
        assert_eq!(res.peers, None);
    }
}
