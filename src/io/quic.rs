use futures::{
    stream::{self, BoxStream},
    StreamExt,
};
use std::{
    collections::HashMap,
    fmt,
    future::{poll_fn, Future},
    task::{Context, Poll},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug};

use super::ConnId;
use crate::{
    proto::{
        data::{
            AnnounceRequest, AnnounceResponse, ControlMessage, FrameMeta, QueryRequest,
            QueryResponse, Request, Response,
        },
        wire::{Frame},
    },
    queue::SendQueue,
    stream::{
        accept_bi, accept_uni, AcceptStream,
        RecvStreams, StreamAddr, StreamError,
    },
};

pub use client::*;
pub use server::*;

pub const WAHT_ALPN: &[u8] = b"n0/waht/0";

pub mod tls;

fn spawn<Fut: Future<Output = Result<(), anyhow::Error>> + Send + 'static>(
    fut: Fut,
) -> tokio::task::JoinHandle<Fut::Output> {
    tokio::spawn(fut)
}

pub trait Incoming: Default {
    type Item: fmt::Debug;
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<FrameIncoming<Self::Item>, StreamError>>;
}

#[derive(Default)]
struct ServerIncoming {
    control: RecvStreams<ControlMessage>,
    announce: RecvStreams<AnnounceRequest>,
    query: RecvStreams<QueryRequest>,
}

impl Incoming for ServerIncoming {
    type Item = Request;
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<FrameIncoming<Request>, StreamError>> {
        if let Poll::Ready(Some(frame)) = self.control.poll_next_as_request(cx) {
            return Poll::Ready(frame);
        }
        if let Poll::Ready(Some(frame)) = self.announce.poll_next_as_request(cx) {
            return Poll::Ready(frame);
        }
        if let Poll::Ready(Some(frame)) = self.query.poll_next_as_request(cx) {
            return Poll::Ready(frame);
        }
        Poll::Pending
    }
}

#[derive(Default)]
struct ClientIncoming {
    control: RecvStreams<ControlMessage>,
    announce: RecvStreams<AnnounceResponse>,
    query: RecvStreams<QueryResponse>,
}

impl Incoming for ClientIncoming {
    type Item = Response;
    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<FrameIncoming<Response>, StreamError>> {
        if let Poll::Ready(Some(frame)) = self.control.poll_next_as_response(cx) {
            return Poll::Ready(frame);
        }
        if let Poll::Ready(Some(frame)) = self.announce.poll_next_as_response(cx) {
            return Poll::Ready(frame);
        }
        if let Poll::Ready(Some(frame)) = self.query.poll_next_as_response(cx) {
            return Poll::Ready(frame);
        }
        Poll::Pending
    }
}

#[derive(Debug)]
pub struct FrameIncoming<T: fmt::Debug> {
    payload: T,
    meta: FrameMeta,
    stream: StreamAddr,
}
impl<T: fmt::Debug> FrameIncoming<T> {
    pub fn new(payload: T, meta: FrameMeta, stream: StreamAddr) -> Self {
        Self {
            payload,
            meta,
            stream,
        }
    }
}

#[derive(Default)]
struct Outgoing {
    control: SendQueue,
    announce: SendQueue,
    query: SendQueue,
}

impl Outgoing {
    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        let mut ready = false;
        while let Poll::Ready(res) = self.control.poll_send(cx) {
            res?;
            ready = true;
        }
        while let Poll::Ready(res) = self.query.poll_send(cx) {
            res?;
            ready = true;
        }
        while let Poll::Ready(res) = self.announce.poll_send(cx) {
            res?;
            ready = true;
        }
        match ready {
            true => Poll::Ready(Ok(())),
            false => Poll::Pending,
        }
    }

    fn len(&self) -> usize {
        self.control.len() + self.announce.len() + self.query.len()
    }
}

#[derive(Default)]
pub struct Accept {
    streams: stream::SelectAll<BoxStream<'static, (ConnId, Result<AcceptStream, StreamError>)>>,
}

impl Accept {
    pub fn push_conn(&mut self, conn: quinn::Connection, cancel: CancellationToken) {
        let accept_bi = stream::unfold((conn.clone(), cancel.clone()), |(conn, cancel)| async {
            tokio::select! {
                _ = cancel.cancelled() => None,
                s = accept_bi(&conn, Some(cancel.clone())) => Some(((conn.stable_id(), s), (conn, cancel)))
            }
        })
        .boxed();
        let accept_uni = stream::unfold((conn.clone(), cancel.clone()), |(conn, cancel)| async {
            tokio::select! {
                _ = cancel.cancelled() => None,
                s = accept_uni(&conn, Some(cancel.clone())) => Some(((conn.stable_id(), s), (conn, cancel)))
            }
        })
        .boxed();
        self.streams.push(accept_bi);
        self.streams.push(accept_uni);
    }

    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<(ConnId, Result<AcceptStream, StreamError>)> {
        if let Poll::Ready(Some(stream)) = self.streams.poll_next_unpin(cx) {
            Poll::Ready(stream)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
pub enum ConnEvent2<T: fmt::Debug> {
    ConnOpen(ConnId),
    ConnClose(ConnId),
    FrameIncoming(FrameIncoming<T>),
}

pub struct ConnState {
    conn: quinn::Connection,
    cancel: CancellationToken,
}

pub struct IncomingActor {
    conns: HashMap<ConnId, ConnState>,
    accept: BoxStream<'static, Result<quinn::Connection, StreamError>>,
    accept_streams: Accept,
    incoming: ServerIncoming,
    outgoing: Outgoing,
}

impl IncomingActor {
    pub fn new(endpoint: quinn::Endpoint) -> Self {
        let accept = stream::unfold(endpoint, |endpoint| async {
            match endpoint.accept().await {
                Some(conn) => Some((conn.await.map_err(StreamError::ConnectionError), endpoint)),
                None => None,
            }
        })
        .boxed();
        Self {
            accept,
            conns: HashMap::new(),
            accept_streams: Accept::default(),
            incoming: ServerIncoming::default(),
            outgoing: Outgoing::default(),
        }
    }

    fn conn(&self, conn_id: &ConnId) -> Option<&ConnState> {
        self.conns.get(conn_id)
    }

    fn poll_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<ConnEvent2<Request>, StreamError>> {
        self.poll_accept(cx)?;
        self.poll_accept_streams(cx)?;
        self.poll_send(cx)?;
        if let Poll::Ready(frame) = self.poll_recv(cx)? {
            return Poll::Ready(Ok(ConnEvent2::FrameIncoming(frame)));
        }
        Poll::Pending
    }

    pub async fn next(&mut self) -> Result<ConnEvent2<Request>, StreamError> {
        poll_fn(|cx| self.poll_next(cx)).await
    }

    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Result<(), StreamError> {
        while let Poll::Ready(Some(conn)) = self.accept.poll_next_unpin(cx) {
            match conn {
                Ok(conn) => self.push_conn(conn),
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn poll_send(&mut self, cx: &mut Context<'_>) -> Result<(), StreamError> {
        while let Poll::Ready(res) = self.outgoing.poll_send(cx) {
            res?;
        }
        Ok(())
    }

    fn poll_accept_streams(&mut self, cx: &mut Context<'_>) -> Result<(), StreamError> {
        while let Poll::Ready((conn_id, stream)) = self.accept_streams.poll_accept(cx) {
            let stream = stream;
            match stream {
                Err(err) => {
                    debug!("stream error on conn {conn_id}: {err:?}");
                    if let Some(conn) = self.conns.remove(&conn_id) {
                        conn.cancel.cancel();
                    }
                }
                Ok(AcceptStream::Control(recv)) => self.incoming.control.push_stream(recv),
                Ok(AcceptStream::Announce((send, recv))) => {
                    self.incoming.announce.push_stream(recv);
                    self.outgoing.announce.push_stream(send);
                }
                Ok(AcceptStream::Query((send, recv))) => {
                    self.incoming.query.push_stream(recv);
                    self.outgoing.query.push_stream(send);
                }
            }
        }
        Ok(())
    }

    fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<FrameIncoming<Request>, StreamError>> {
        let res = self.incoming.poll_recv(cx);
        res
    }

    fn reply_query(&mut self, addr: &StreamAddr, response: Frame) {
        self.outgoing.query.push_frame(*addr, response)
    }

    fn push_conn(&mut self, conn: quinn::Connection) {
        let cancel = CancellationToken::new();
        self.accept_streams.push_conn(conn.clone(), cancel.clone());
        self.conns
            .insert(conn.stable_id(), ConnState { conn, cancel });
    }
}

pub mod client {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use quinn::Endpoint;
    use tracing::{debug, error};

    use super::tls::configure_client;
    use crate::{
        // io::quic::spawn,
        proto::{
            data::QueryResponse,
            wire::{Frame, Query, UnsignedFrame},
        },
        stream::{open::QueryBiStream, open_query, StreamError},
        // tracker::client::{ClientActor, ClientHandle},
        util::resolve_tracker_url,
    };

    #[derive(Clone, Debug)]
    pub struct Config {
        pub tls_accept_insecure: bool,
    }
    impl Config {
        pub fn accept_insecure() -> Self {
            Self {
                tls_accept_insecure: true,
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Client {
        conn: quinn::Connection,
    }

    pub struct ClientQuery(QueryBiStream);
    impl ClientQuery {
        pub async fn next(&mut self) -> Option<Result<QueryResponse, StreamError>> {
            let item = self.0 .1.recv().await;
            match item {
                Err(err) => Some(Err(err.into())),
                Ok(None) => None,
                Ok(Some((res, meta))) => Some(Ok(res)),
            }
        }
    }

    impl Client {
        pub async fn connect(tracker_url: &str, config: Config) -> anyhow::Result<Self> {
            let tracker_addr = resolve_tracker_url(tracker_url).await?;
            debug!("resolved tracker url {tracker_url} to {tracker_addr}");
            // let (peer_id, _signing_key) = generate_peer_id();
            let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
            let mut endpoint = Endpoint::client(bind_addr)?;
            endpoint.set_default_client_config(configure_client(config.tls_accept_insecure));
            let conn = endpoint.connect(tracker_addr, "localhost")?.await?;
            Ok(Self { conn })
        }

        pub async fn query(&self, query: Query) -> Result<ClientQuery, StreamError> {
            let stream = open_query(self.conn.clone()).await?;
            let mut handle = ClientQuery(stream);
            let frame = Frame {
                rid: None,
                payload: crate::proto::wire::FramePayload::Unsigned(UnsignedFrame::Query(query)),
            };
            handle.0 .0.send(&frame).await?;
            Ok(handle)
        }
    }

    // pub async fn connect(tracker_url: &str, config: Config) -> anyhow::Result<ClientHandle> {
    //     let tracker_addr = resolve_tracker_url(tracker_url).await?;
    //     debug!("resolved tracker url {tracker_url} to {tracker_addr}");
    //     let (peer_id, _signing_key) = generate_peer_id();
    //     let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
    //     let mut endpoint = Endpoint::client(bind_addr)?;
    //     endpoint.set_default_client_config(configure_client(config.tls_accept_insecure));
    //
    //     let (actor, conn_tx, handle) = ClientActor::create(peer_id);
    //     tokio::spawn(async move {
    //         match actor.run().await {
    //             Err(err) => error!("client state actor failed: {err:?}"),
    //             Ok(()) => debug!("client state actor closed"),
    //         }
    //     });
    //
    //     let conn = endpoint.connect(tracker_addr, "localhost")?;
    //     spawn(async move {
    //         let conn = conn.await?;
    //
    //         // match conn_actor(conn, conn_tx.clone(), true).await {
    //         //     Err(err) => error!("client conn actor failed: {err:?}"),
    //         //     Ok(()) => debug!("client conn actor closed"),
    //         // }
    //         Ok(())
    //     });
    //     Ok(handle)
    // }
}

pub mod server {
    use std::{net::SocketAddr, sync::Arc};

    use ed25519_dalek::SigningKey;
    use quinn::Endpoint;

    use crate::io::quic::{ConnEvent2, IncomingActor};
    use crate::proto::crypto::SealedEnvelope;
    use crate::proto::data::Request;
    use crate::proto::wire::{
        AnnouncePayload, Announcement, Frame, FramePayload, ProvideHash, SignedFrame,
    };
    use crate::tracker::server::TrackerConfig;

    use super::tls;

    const MAX_CONNECTIONS: u32 = 4096;
    const MAX_STREAMS: u32 = 10;

    #[derive(Debug)]
    pub struct Config {
        pub tracker: TrackerConfig,
        pub tls_server_name: String,
        pub tls_self_signed: bool,
    }
    impl Default for Config {
        fn default() -> Self {
            Self {
                tracker: TrackerConfig::default(),
                tls_server_name: "localhost".into(),
                tls_self_signed: true,
            }
        }
    }

    pub async fn listen(bind_addr: SocketAddr, config: Config) -> anyhow::Result<()> {
        // let (peer_id, _signing_key) = generate_peer_id();
        // let state = ServerState::with_config(peer_id, config.tracker);
        // let (state_actor, from_conn_tx) = ServerActor::create(state);
        // let state_handle = tokio::spawn(state_actor.run());

        let server_config = tls_server_config(config.tls_server_name, config.tls_self_signed)?;
        let endpoint = bind_server_endpoint(bind_addr, server_config)?;
        let handle = server_actor(endpoint);

        handle.await?;
        // state_handle.await??;

        Ok(())
    }

    async fn server_actor(endpoint: Endpoint) -> anyhow::Result<()> {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng {});
        let mut actor = IncomingActor::new(endpoint);
        loop {
            let event = actor.next().await?;
            match event {
                ConnEvent2::FrameIncoming(frame) => match frame.payload {
                    Request::Control(_) => todo!(),
                    Request::Announce(_) => todo!(),
                    Request::Query(_query) => {
                        // THIS IS SIMULATION
                        // instead this would return announce frames as signed by peers
                        // hacking something in here to test initial data flow
                        let payload = &SignedFrame::Announce(Announcement {
                            timestamp: 0,
                            payload: AnnouncePayload::ProvideHash(ProvideHash {
                                target: [9u8; 32],
                            }),
                        });
                        let res = Frame {
                            rid: frame.meta.rid,
                            payload: FramePayload::Signed(SealedEnvelope::sign_and_encode(
                                &signing_key,
                                payload,
                            )?),
                        };
                        actor.reply_query(&frame.stream, res);
                    }
                },
                _ => {}
            }
        }
    }

    fn tls_server_config(
        server_name: String,
        self_signed: bool,
    ) -> anyhow::Result<quinn::ServerConfig> {
        if !self_signed {
            anyhow::bail!("ACME certificate generation is not yet supported");
        }
        let tls_server_config = tls::make_selfsigned_server_config(false, server_name)?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_concurrent_bidi_streams(MAX_STREAMS.into())
            .max_concurrent_uni_streams(0u32.into());

        server_config
            .transport_config(Arc::new(transport_config))
            .concurrent_connections(MAX_CONNECTIONS);
        Ok(server_config)
    }

    fn bind_server_endpoint(
        bind_addr: SocketAddr,
        server_config: quinn::ServerConfig,
    ) -> anyhow::Result<Endpoint> {
        let socket = socket2::Socket::new(
            // TODO: Support IPV6
            socket2::Domain::IPV4,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        socket.set_reuse_port(true)?;
        socket.bind(&bind_addr.into())?;

        let endpoint = quinn::Endpoint::new(
            Default::default(),
            Some(server_config),
            socket.into(),
            Arc::new(quinn::TokioRuntime {}),
        )?;
        Ok(endpoint)
    }
}
