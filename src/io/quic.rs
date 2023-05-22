use anyhow::Context;
pub use client::*;
use quinn::Connecting;
pub use server::*;
use tokio::sync::mpsc::Sender;

use crate::io::write_message;

use super::{read_message, ConnEvent, ConnHandle};

pub const WAHT_ALPN: &[u8] = b"n0/waht/0";

pub mod tls;

pub async fn conn_actor(
    conn: Connecting,
    tx: Sender<ConnEvent>,
    is_initiator: bool,
) -> anyhow::Result<()> {
    let conn = conn.await?;
    let conn_id = conn.stable_id();

    let (mut send, mut recv) = match is_initiator {
        true => conn.open_bi().await?,
        false => conn.accept_bi().await?,
    };

    let (handle, mut rx, cancel) = ConnHandle::new(conn_id, conn.remote_address());
    tx.send(ConnEvent::Open(conn_id, handle)).await?;

    let mut write_buf = vec![];
    let cancel_clone = cancel.clone();
    let send_loop = tokio::spawn(async move {
        loop {
            tokio::select! {
                // allow to cancel the loop
                _ = cancel_clone.cancelled() => break,
                // write outgoing messages
                message = rx.recv() => {
                    match message {
                        Some(message) => write_message(&mut send, &message, &mut write_buf).await.context("write loop errored")?,
                        None =>  cancel_clone.cancel(),
                    }
                }
            }
        }
        Ok(())
    });

    let tx_clone = tx.clone();
    let mut read_buf = vec![];
    let recv_loop = tokio::spawn(async move {
        loop {
            tokio::select! {
                // allow to cancel the loop
                _ = cancel.cancelled() => break,
                // read and forward incoming messages
                message = read_message(&mut recv, &mut read_buf) => {
                    let message = message?;
                    if let Err(_channel_closed) = tx_clone
                        .send(ConnEvent::Packet(conn_id, message))
                        .await
                    {
                        cancel.cancel();
                    }
                }
            }
        }
        Ok(())
    });

    let (res, _, _) = futures::future::select_all([send_loop, recv_loop]).await;
    let res: anyhow::Result<()> = res?;

    tx.send(ConnEvent::Close(conn_id)).await?;
    res
}

pub mod client {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use quinn::Endpoint;
    use tracing::{debug, error};

    use super::conn_actor;
    use super::tls::configure_client;
    use crate::{
        tracker::client::{ClientActor, ClientHandle},
        util::generate_peer_id,
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

    pub async fn connect(tracker_addr: SocketAddr, config: Config) -> anyhow::Result<ClientHandle> {
        let (peer_id, _signing_key) = generate_peer_id();
        let bind_addr: SocketAddr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0).into();
        let mut endpoint = Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(configure_client(config.tls_accept_insecure));

        let (actor, conn_tx, handle) = ClientActor::create(peer_id);
        tokio::spawn(async move {
            match actor.run().await {
                Err(err) => error!("client state actor failed: {err:?}"),
                Ok(()) => debug!("client state actor closed"),
            }
        });

        let conn = endpoint.connect(tracker_addr, "localhost")?;
        tokio::spawn(async move {
            match conn_actor(conn, conn_tx.clone(), true).await {
                Err(err) => error!("client conn actor failed: {err:?}"),
                Ok(()) => debug!("client conn actor closed"),
            }
        });
        Ok(handle)
    }
}

pub mod server {
    use std::{net::SocketAddr, sync::Arc};

    use quinn::Endpoint;
    use tokio::sync::mpsc::Sender;
    use tracing::{debug, info};

    use crate::io::ConnEvent;
    use crate::tracker::server::{ServerActor, ServerState, TrackerConfig};
    use crate::util::generate_peer_id;

    use super::{conn_actor, tls};

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
        let (peer_id, _signing_key) = generate_peer_id();
        let state = ServerState::with_config(peer_id, config.tracker);

        let (state_actor, from_conn_tx) = ServerActor::create(state);
        let state_handle = tokio::spawn(state_actor.run());

        let server_config = tls_server_config(config.tls_server_name, config.tls_self_signed)?;
        let endpoint = bind_server_endpoint(bind_addr, server_config)?;
        let endpoint_handle = endpoint_actor(endpoint, from_conn_tx);

        endpoint_handle.await?;
        state_handle.await??;

        Ok(())
    }

    async fn endpoint_actor(endpoint: Endpoint, tx: Sender<ConnEvent>) -> anyhow::Result<()> {
        info!("server listening on quic://{}", endpoint.local_addr()?);
        while let Some(conn) = endpoint.accept().await {
            let from_addr = conn.remote_address();
            debug!("connection incoming from {from_addr}");
            let fut = conn_actor(conn, tx.clone(), false);
            tokio::spawn(async move {
                let res = fut.await;
                let reason = res
                    .map(|_| String::new())
                    .unwrap_or_else(|err| format!(": {}", err.to_string()));
                debug!("connection closed from {from_addr}{reason}");
            });
        }
        Ok(())
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
