use std::time::Duration;

use waht::{
    io::quic::{listen, Client},
    proto::wire::{Query, QueryKeys, QueryPayload, QuerySettings},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let server_handle = tokio::spawn(run_server());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client_handle = tokio::spawn(run_client());
    client_handle.await.unwrap().unwrap();
    server_handle.await.unwrap().unwrap();
    Ok(())
}

async fn run_server() -> anyhow::Result<()> {
    let addr = "0.0.0.0:1234".parse()?;
    listen(addr, Default::default()).await?;
    Ok(())
}

async fn run_client() -> anyhow::Result<()> {
    let addr = "quic://localhost:1234";
    let client = Client::connect(addr, waht::io::quic::client::Config::accept_insecure()).await?;
    let req = Query {
        settings: QuerySettings {
            federate: true,
            keepalive: false,
            stop_after: 6,
        },
        query: QueryPayload::Keys(QueryKeys {
            keys: vec![[7u8; 32]],
        }),
    };
    let mut stream = client.query(req).await?;
    let res = stream.next().await;
    eprintln!("client res: {res:?}");
    Ok(())
}
