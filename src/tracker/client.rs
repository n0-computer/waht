use anyhow::{anyhow, Context};
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::Interval;
use tracing::{debug, warn};

use crate::{
    io::{ConnEvent, ConnHandle, ConnId, FROM_CONN_CHANNEL_CAP},
    tracker::{
        proto::{self, Command, Message, Payload, Request, Response},
        PeerId, RequestId,
    },
    util::time::InstantMap,
};

pub const REQUEST_TTL: Duration = Duration::from_secs(30);
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);
pub const RESPONSE_CHANNEL_CAP: usize = 4;

pub type TrackerAddr = SocketAddr;
pub type CommandRx = Receiver<(Command, ResponseTx)>;
pub type ResponseRx = Receiver<anyhow::Result<Response>>;
pub type ResponseTx = Sender<anyhow::Result<Response>>;

#[derive(Clone, Debug)]
pub struct ClientHandle {
    command_tx: Sender<(Command, ResponseTx)>,
}

impl ClientHandle {
    pub fn new() -> (Self, CommandRx) {
        let (command_tx, command_rx) = mpsc::channel(64);
        (Self { command_tx }, command_rx)
    }
    pub async fn request(&self, command: Command) -> anyhow::Result<ResponseRx> {
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_CAP);
        self.command_tx.send((command, tx)).await?;
        Ok(rx)
    }

    pub async fn request_one(&self, command: Command) -> anyhow::Result<Response> {
        let mut rx = self.request(command).await?;
        let next = rx
            .recv()
            .await
            .context("channel closed before a response was received");
        let res = next?;
        res
    }
}

pub struct ClientActor {
    peer_id: PeerId,
    conn_rx: Receiver<ConnEvent>,
    command_rx: CommandRx,
    conns: HashMap<ConnId, ConnHandle>,
    requests: RequestMap,
    tick_interval: Interval,
}

impl ClientActor {
    pub fn new(peer_id: PeerId, conn_rx: Receiver<ConnEvent>, command_rx: CommandRx) -> Self {
        Self {
            peer_id,
            tick_interval: tokio::time::interval(TICK_INTERVAL),
            command_rx,
            conn_rx,
            requests: Default::default(),
            conns: Default::default(),
        }
    }
    pub fn create(peer_id: PeerId) -> (ClientActor, Sender<ConnEvent>, ClientHandle) {
        let (conn_tx, conn_rx) = mpsc::channel(FROM_CONN_CHANNEL_CAP);
        let (handle, command_rx) = ClientHandle::new();
        let actor = ClientActor::new(peer_id, conn_rx, command_rx);
        (actor, conn_tx, handle)
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        if self.conns.is_empty() {
            let event = self
                .conn_rx
                .recv()
                .await
                .context("failed to recv first message")?;
            if let Err(err) = self.handle_conn_event(event).await {
                warn!("Error while handling connection event: {err:?}");
            }
        }
        loop {
            tokio::select! {
                _ = self.tick_interval.tick() => {
                    self.handle_timeout(Instant::now()).await;
                    self.tick_interval.reset();
                }
                command = self.command_rx.recv() => {
                    debug!("client recv command: {command:?}");
                    match command {
                        Some(command) => self.handle_command(command).await.context("failed to handle command")?,
                        None => {
                            warn!("command sender dropped! abort");
                            break Ok(())
                        }
                    }
                }
                event = self.conn_rx.recv() => {
                    debug!("client recv conn event: {event:?}");
                    match event {
                        Some(event) => self.handle_conn_event(event).await.context("failed to handle conn event")?,
                        None => {
                            warn!("conn actor dropped! abort");
                            break Ok(())
                        }
                    }
                }
            }
        }
    }

    async fn handle_conn_event(&mut self, event: ConnEvent) -> anyhow::Result<()> {
        debug!("conn event: {event:?}");
        match event {
            ConnEvent::Open(conn_id, handle) => {
                self.conns.insert(conn_id, handle);
                let command = proto::Command::Hello(proto::Hello {});
                self.send_request(command, None, Some(conn_id)).await?;
            }
            ConnEvent::Packet(conn_id, message) => match message.payload {
                Payload::Response(response) => {
                    debug!(conn_id, "conn recv response: {response:?}");
                    let request_id = response.request_id;
                    match self.requests.get(&request_id) {
                        Some(state) if state.conn_id == conn_id => {
                            self.requests.forward_response(response).await;
                        }
                        Some(_state) => {
                            // TODO: abort conn?
                            warn!(conn_id, "received response from invalid conn");
                        }
                        None => {
                            // TODO: abort conn?
                            warn!(conn_id, "response with invalid request_id {request_id}");
                        }
                    };
                }
                _ => {
                    // TODO: abort conn
                    warn!(conn_id, "received invalid packet: expected a response");
                }
            },
            ConnEvent::Close(conn_id) => {
                self.conns.remove(&conn_id);
                self.requests.remove_by_conn(&conn_id);
            }
        }
        Ok(())
    }

    async fn handle_command(
        &mut self,
        (command, response_tx): (Command, ResponseTx),
    ) -> anyhow::Result<()> {
        self.send_request(command, Some(response_tx), None).await
    }

    fn select_conn(&self) -> anyhow::Result<ConnId> {
        self.conns
            .keys()
            .next()
            .map(|x| *x)
            .ok_or_else(|| anyhow!("no connections to trackers"))
    }

    async fn send_request(
        &mut self,
        command: Command,
        response_tx: Option<ResponseTx>,
        conn_id: Option<ConnId>,
    ) -> anyhow::Result<()> {
        let conn_id = match conn_id {
            Some(conn_id) => conn_id,
            None => self.select_conn().context("no connection available")?,
        };
        let request_id = self.requests.next_id();
        let request = Request {
            command,
            peer_info: None,
            request_id,
        };
        let message = Message::request(self.peer_id, request);
        let state = RequestState::new(request_id, conn_id, response_tx);
        self.requests.insert(request_id, state);
        let conn = self
            .conns
            .get(&conn_id)
            .ok_or_else(|| anyhow!("connection id is invalid"))?;
        debug!(conn_id, "send request: {message:?}");
        match conn.send(message).await {
            Ok(()) => Ok(()),
            Err(err) => Err(err.context("conn send fail: missing conn actor")),
        }
    }

    async fn handle_timeout(&mut self, now: Instant) {
        for state in self.requests.drain_expired(now) {
            if let Some(tx) = state.response_tx {
                let err = anyhow!("request timed out");
                tx.send_timeout(Err(err), Duration::from_millis(10))
                    .await
                    .ok();
            }
        }
    }
}

/// State for a single request. Keeps metadata and the Sender for responses.
#[derive(Debug)]
pub struct RequestState {
    response_tx: Option<ResponseTx>,
    ttl: Instant,
    conn_id: ConnId,
}

impl RequestState {
    pub fn new(_request_id: RequestId, conn_id: ConnId, response_tx: Option<ResponseTx>) -> Self {
        let now = Instant::now();
        Self {
            ttl: now + REQUEST_TTL,
            response_tx,
            conn_id,
        }
    }

    pub async fn forward_response(&mut self, response: anyhow::Result<Response>) -> bool {
        let now = Instant::now();
        self.ttl = now + REQUEST_TTL;
        if let Some(tx) = &mut self.response_tx {
            match tx.send(response).await {
                Ok(_) => true,
                Err(_) => false,
            }
        } else {
            false
        }
    }
}

/// Table of all running requests. Also maintains a sorted list of requests by TTL.
pub struct RequestMap {
    next_request_id: RequestId,
    requests: HashMap<RequestId, RequestState>,
    by_ttl: InstantMap<RequestId>,
}

impl Default for RequestMap {
    fn default() -> Self {
        Self {
            next_request_id: 1,
            requests: Default::default(),
            by_ttl: Default::default(),
        }
    }
}

impl RequestMap {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn next_id(&mut self) -> RequestId {
        let next_id = self.next_request_id;
        self.next_request_id += 1;
        next_id
    }

    pub fn insert(&mut self, id: RequestId, request: RequestState) {
        self.by_ttl.insert(request.ttl, id);
        self.requests.insert(id, request);
    }

    pub fn get(&self, id: &RequestId) -> Option<&RequestState> {
        self.requests.get(id)
    }

    pub fn remove_by_conn(&mut self, conn_id: &ConnId) {
        self.requests.retain(|id, request| {
            let retain = request.conn_id != *conn_id;
            if !retain {
                self.by_ttl.remove(request.ttl, id);
            }
            retain
        });
    }

    pub async fn forward_response(&mut self, response: Response) -> Option<RequestState> {
        let id = response.request_id;
        let fin = response.fin;
        if let Some(request) = self.requests.get_mut(&id) {
            let old_ttl = request.ttl;
            request.forward_response(Ok(response)).await;
            if fin {
                return self.remove(&id);
            } else {
                self.by_ttl.update(old_ttl, request.ttl, id);
            }
        }
        None
    }

    pub fn remove(&mut self, id: &RequestId) -> Option<RequestState> {
        let request = self.requests.remove(id);
        match request {
            Some(request) => {
                self.by_ttl.remove(request.ttl, id);
                Some(request)
            }
            None => None,
        }
    }

    /// Remove and return all requests whose TTL is lower than the passed in Instant.
    pub fn drain_expired(&mut self, now: Instant) -> impl Iterator<Item = RequestState> + '_ {
        self.by_ttl
            .drain_expired(now)
            .map(|id| self.requests.remove(&id))
            .flatten()
    }
}
