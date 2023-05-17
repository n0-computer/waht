use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use crate::util::Interval;

use super::{
    proto::{Command, Message, Payload, Request, Response},
    PeerId, PeerInfo, RequestId, TrackerInfo,
};
use anyhow::anyhow;

pub use actor::{ClientActor, ClientHandle};

pub type TrackerAddr = SocketAddr;

pub const REQUEST_TTL: Duration = Duration::from_secs(30);
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct ClientState {
    peer_id: PeerId,
    info: PeerInfo,
    info_changed: bool,
    requests: HashMap<RequestId, RequestState>,
    next_request_id: RequestId,
    tracker: TrackerInfo, // trackers: Vec<TrackerState>,
    tick_interval: Interval,
}

#[derive(Debug)]
pub struct RequestState {
    sent: Instant,
    last_response: Option<Instant>,
}

pub struct RequestOut {
    pub request_id: RequestId,
    pub message: Message,
}

impl ClientState {
    pub fn new(tracker: TrackerInfo, peer_id: PeerId, info: PeerInfo) -> Self {
        Self {
            peer_id,
            info,
            info_changed: true,
            requests: Default::default(),
            next_request_id: 1,
            tracker,
            tick_interval: Interval::from_now(TICK_INTERVAL),
        }
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        if self.tick_interval.has_elapsed(now) {
            // TODO: Optimize
            self.requests.retain(|_id, state| {
                let last_event = state.last_response.unwrap_or(state.sent);
                now > last_event + REQUEST_TTL
            })
        }
    }

    pub fn poll_timeout(&self) -> Instant {
        self.tick_interval.next()
    }

    pub fn remote_addr(&self) -> &SocketAddr {
        &self.tracker.addr
    }
    pub fn update_info(&mut self, info: PeerInfo) {
        if self.info != info {
            self.info = info;
            self.info_changed = true;
        }
    }
    fn next_request_id(&mut self) -> RequestId {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }
    fn info_if_changed(&mut self) -> Option<PeerInfo> {
        match self.info_changed {
            false => None,
            true => {
                self.info_changed = false;
                Some(self.info.clone())
            }
        }
    }

    fn prepare_request(&mut self, command: Command) -> Request {
        let request_id = self.next_request_id();
        let state = RequestState {
            sent: Instant::now(),
            last_response: None,
        };
        self.requests.insert(request_id, state);
        Request {
            request_id,
            command,
            peer_info: self.info_if_changed(),
        }
    }

    pub fn request(&mut self, command: Command) -> RequestOut {
        let request = self.prepare_request(command);
        let request_id = request.request_id;
        let message = Message::request(self.peer_id, request);
        RequestOut {
            request_id,
            message,
        }
    }

    pub fn handle_message(&mut self, message: Message) -> anyhow::Result<Response> {
        match message.payload {
            Payload::Response(response) => self
                .handle_response(response)
                .ok_or_else(|| anyhow!("Invalid request ID")),
            Payload::Request(_request) => Err(anyhow!("Expected a reponse, but got a request")),
            Payload::Error(err) => Err(anyhow!("Remote error: {err:?}")),
        }
    }

    pub fn handle_response(&mut self, response: Response) -> Option<Response> {
        if let Some(state) = self.requests.get_mut(&response.request_id) {
            if !response.fin {
                state.last_response = Some(Instant::now());
            } else {
                self.requests.remove(&response.request_id);
            }
            Some(response)
        } else {
            None
        }
    }
}

mod actor {
    use flume::{Receiver, Sender};
    use std::{collections::HashMap, pin::Pin, time::Instant};
    use tokio::time::sleep_until;
    use tracing::{debug, log::warn};

    use crate::{
        io::{ConnEvent, ConnHandle, ConnId, FromConn},
        tracker::{
            proto::{self, Command, Response},
            RequestId,
        },
    };

    use super::ClientState;

    #[derive(Debug)]
    pub struct ClientHandle {
        command_tx: Sender<(Command, Sender<anyhow::Result<Response>>)>,
    }
    pub type CommandsRx = Receiver<(Command, Sender<anyhow::Result<Response>>)>;

    impl ClientHandle {
        pub fn new() -> (Self, CommandsRx) {
            let (command_tx, command_rx) = flume::bounded(64);
            (Self { command_tx }, command_rx)
        }
        pub async fn request(
            &self,
            command: Command,
        ) -> anyhow::Result<Receiver<anyhow::Result<Response>>> {
            let (tx, rx) = flume::bounded(2);
            self.command_tx.send_async((command, tx)).await?;
            Ok(rx)
        }
        pub async fn request_one(&self, command: Command) -> anyhow::Result<Response> {
            let stream = self.request(command).await?;
            let next = stream.recv_async().await;
            let res = next?;
            res
        }
    }

    pub struct ClientActor {
        state: ClientState,
        conn_rx: Receiver<ConnEvent>,
        command_rx: Receiver<(Command, Sender<anyhow::Result<Response>>)>,
        conns: HashMap<ConnId, ConnHandle>,
        requests: HashMap<RequestId, Sender<anyhow::Result<Response>>>,
        timeout: Pin<Box<tokio::time::Sleep>>,
    }

    impl ClientActor {
        pub fn new(
            state: ClientState,
            conn_rx: Receiver<ConnEvent>,
            command_rx: Receiver<(Command, Sender<anyhow::Result<Response>>)>,
        ) -> Self {
            Self {
                timeout: Box::pin(sleep_until(state.poll_timeout().into())),
                state,
                command_rx,
                conn_rx,
                requests: Default::default(),
                conns: Default::default(),
            }
        }
        pub fn create(state: ClientState) -> (ClientActor, FromConn, ClientHandle) {
            let (conn_tx, conn_rx) = flume::bounded(2);
            let (handle, command_rx) = ClientHandle::new();
            let actor = ClientActor::new(state, conn_rx, command_rx);
            (actor, conn_tx, handle)
        }
        pub async fn handle_conn_event(&mut self, event: ConnEvent) -> anyhow::Result<()> {
            debug!("conn event: {event:?}");
            match event {
                ConnEvent::Open(handle) => {
                    let command = proto::Command::Hello(proto::Hello {});
                    let request = self.state.request(command);
                    handle.send(request.message).await?;
                    self.conns.insert(handle.conn_id, handle);
                }
                ConnEvent::Packet(packet) => {
                    let response = self.state.handle_message(packet.message)?;
                    let fin = response.fin;
                    let request_id = response.request_id;
                    if let Some(tx) = self.requests.get(&request_id) {
                        // eprintln!("[client state] now send conn->state");
                        tx.send_async(Ok(response)).await?;
                        // eprintln!("[client state] did send conn->state");
                    }
                    if fin {
                        self.requests.remove(&request_id);
                    }
                }
                ConnEvent::Close(conn_id, _addr) => {
                    self.conns.remove(&conn_id);
                }
            }
            Ok(())
        }

        pub async fn handle_command(
            &mut self,
            command: (Command, Sender<anyhow::Result<Response>>),
        ) -> anyhow::Result<()> {
            let (command, res_tx) = command;
            let request = self.state.request(command);
            self.requests.insert(request.request_id, res_tx);

            // TODO: Select which conn to use.
            let conn = self.conns.iter_mut().next();
            if let Some((_peer_id, conn)) = conn {
                conn.send(request.message).await?;
            }
            Ok(())
        }

        pub async fn run(mut self) -> anyhow::Result<()> {
            if self.conns.is_empty() {
                let event = self.conn_rx.recv_async().await?;
                if let Err(err) = self.handle_conn_event(event).await {
                    warn!("Error while handling connection event: {err:?}");
                }
            }
            loop {
                tokio::select! {
                    () = &mut self.timeout => {
                        let now = Instant::now();
                        self.state.handle_timeout(now);
                        self.timeout.as_mut().reset(self.state.poll_timeout().into());
                    }
                    command = self.command_rx.recv_async() => {
                        self.handle_command(command?).await?;
                    }
                    event = self.conn_rx.recv_async() => {
                        self.handle_conn_event(event?).await?;
                    }
                }
            }
        }
    }
}
