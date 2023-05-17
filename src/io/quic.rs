pub use client::*;
use flume::Sender;
use quinn::Connecting;
pub use server::*;
use tracing::{debug, warn};

use crate::io::write_message;

use super::{read_message, ConnEvent, ConnHandle, PacketIn};

pub const WAHT_ALPN: &[u8] = b"n0/waht/0";

pub mod tls;

pub async fn handle_conn(
    conn: Connecting,
    in_tx: Sender<ConnEvent>,
    is_initiator: bool,
) -> anyhow::Result<()> {
    let mut write_buf = vec![];
    let mut read_buf = vec![];

    let conn = conn.await?;

    let (mut send, mut recv) = match is_initiator {
        true => conn.open_bi().await?,
        false => conn.accept_bi().await?,
    };

    let conn_id = conn.stable_id();

    let (handle, out_rx) = ConnHandle::new(conn_id, conn.remote_address());
    in_tx.send_async(ConnEvent::Open(handle)).await?;

    // write loop
    let writer = tokio::spawn(async move {
        loop {
            let message = out_rx.recv_async().await?;
            write_message(&mut send, &message, &mut write_buf).await?;
            debug!("writer: wrote");
        }
    });

    // read loop
    let tx = in_tx.clone();
    let reader = tokio::spawn(async move {
        loop {
            let message = read_message(&mut recv, &mut read_buf).await?;
            if let Some(message) = message {
                tx.send_async(ConnEvent::Packet(PacketIn {
                    // from: peer_id,
                    conn_id,
                    message,
                }))
                .await?;
            } else {
                break;
            }
        }
        Ok(())
    });
    let res: anyhow::Result<()> = {
        let res1 = reader.await?;
        let res2 = writer.await?;
        res1.and_then(|_| res2)
    };

    if let Err(err) = &res {
        warn!(
            "Connection from {} closed with error: {err:?}",
            conn.remote_address()
        );
    }

    in_tx
        .send_async(ConnEvent::Close(conn.stable_id(), conn.remote_address()))
        .await?;

    // TODO: Reason
    conn.close(0u32.into(), "Closed".as_bytes());
    res
}

pub mod client {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use quinn::Endpoint;
    use tracing::{debug, error};

    use super::handle_conn;
    use super::tls::configure_client;
    use crate::{
        tracker::client::{ClientActor, ClientHandle, ClientState},
        util::generate_peer_id,
    };

    pub async fn connect(addr: SocketAddr) -> anyhow::Result<ClientHandle> {
        let (peer_id, _signing_key) = generate_peer_id();
        let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
        let mut endpoint = Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(configure_client());
        // let mut conn = conn.await?;

        let state = ClientState::new(addr.into(), peer_id, Default::default());
        let (actor, conn_tx, handle) = ClientActor::create(state);
        tokio::spawn(async move {
            match actor.run().await {
                Err(err) => error!("client state actor failed: {err}"),
                Ok(()) => debug!("client state actor closed"),
            }
        });

        let conn = endpoint.connect(addr, "localhost")?;
        tokio::spawn(async move {
            match handle_conn(conn, conn_tx, true).await {
                Err(err) => error!("client conn actor failed: {err}"),
                Ok(()) => debug!("client conn actor closed"),
            }
        });
        Ok(handle)
    }
}

pub mod server {
    use std::{net::SocketAddr, sync::Arc};

    use quinn::{Endpoint, ServerConfig};
    use tracing::{debug, error, info};

    use crate::io::FromConn;
    use crate::tracker::server::{ServerActor, ServerState};
    use crate::util::generate_peer_id;

    use super::{handle_conn, tls};

    const MAX_CONNECTIONS: u32 = 4096;
    const MAX_STREAMS: u64 = 10;

    #[derive(Debug, Default)]
    pub struct Config {}

    pub async fn listen(bind_addr: SocketAddr, _config: Config) -> anyhow::Result<()> {
        let (peer_id, _signing_key) = generate_peer_id();
        let state = ServerState::new(peer_id);

        let (actor, in_tx) = ServerActor::create(state);

        let state_handle = tokio::spawn(actor.run());
        let server_config = server_config()?;

        let endpoint = bind_server_endpoint(bind_addr, server_config)?;
        endpoint_actor(endpoint, in_tx).await?;

        state_handle.await??;

        Ok(())
    }

    async fn endpoint_actor(endpoint: Endpoint, in_tx: FromConn) -> anyhow::Result<()> {
        info!("Server listening on quic://{}", endpoint.local_addr()?);

        while let Some(conn) = endpoint.accept().await {
            let from_addr = conn.remote_address();
            debug!("connection incoming from {}", from_addr);
            let fut = handle_conn(conn, in_tx.clone(), false);
            tokio::spawn(async move {
                if let Err(e) = fut.await {
                    error!("connection failed: {reason}", reason = e.to_string())
                }
            });
        }
        Ok(())
    }

    fn server_config() -> anyhow::Result<ServerConfig> {
        let tls_server_config = tls::make_server_config(false)?;
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_server_config));
        let mut transport_config = quinn::TransportConfig::default();
        transport_config
            .max_concurrent_bidi_streams(MAX_STREAMS.try_into()?)
            .max_concurrent_uni_streams(0u32.into());

        server_config
            .transport_config(Arc::new(transport_config))
            .concurrent_connections(MAX_CONNECTIONS);
        Ok(server_config)
    }

    fn bind_server_endpoint(
        bind_addr: SocketAddr,
        server_config: ServerConfig,
    ) -> anyhow::Result<Endpoint> {
        // let socket = std::net::UdpSocket::bind(bind_addr)?;
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
            Arc::new(quinn::TokioRuntime {}), // quinn::default_runtime().expect("no async runtime found"),
        )?;
        Ok(endpoint)
    }
}
