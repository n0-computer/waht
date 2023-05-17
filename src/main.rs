use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use clap::Parser;
use futures::future::select_all;
use rand::seq::SliceRandom;
use waht::{io::quic, tracker::proto};

mod cli {
    use super::*;
    #[derive(Debug, Parser)]
    pub struct Args {
        #[clap(subcommand)]
        pub command: Command,

        #[clap(short, long)]
        pub tracker: Vec<SocketAddr>,
    }

    #[derive(Debug, Parser)]
    pub enum Command {
        Server {
            #[clap(short, long, default_value = "0.0.0.0:1234")]
            bind_addr: SocketAddr,
        },
        Provide {
            topic: String,
        },
        Lookup {
            topic: String,
        },
        LoadTest {
            clients: usize,
        },
    }
}

const DEFAULT_TRACKER: &str = "127.0.0.1:1234";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = cli::Args::parse();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run(cli))?;
    Ok(())
}

async fn run(cli: cli::Args) -> anyhow::Result<()> {
    let tracker_addr = cli
        .tracker
        .get(0)
        .map(|x| *x)
        .unwrap_or_else(|| DEFAULT_TRACKER.parse::<SocketAddr>().unwrap());
    match cli.command {
        cli::Command::Server { bind_addr } => {
            let config = quic::server::Config::default();
            quic::listen(bind_addr, config).await?;
        }
        cli::Command::Provide { topic } => {
            let topic = blake3::hash(topic.as_bytes()).into();
            let command = proto::Command::ProvideTopic(proto::ProvideTopic { topic });
            let client = quic::connect(tracker_addr).await?;
            let res = client.request_one(command).await?;
            eprintln!("{res:?}");
        }
        cli::Command::Lookup { topic } => {
            let topic = blake3::hash(topic.as_bytes()).into();
            let command = proto::Command::LookupTopic(proto::LookupTopic { topic });
            let client = quic::connect(tracker_addr).await?;
            let res = client.request_one(command).await?;
            // eprintln!("Got reply from {}", hex::encode(res.peer_id));
            if let Some(peers) = res.peers {
                eprintln!("Peers:  ");
                for peer in peers {
                    eprintln!(
                        "{}",
                        peer.addrs
                            .into_iter()
                            .map(|x| x.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            } else {
                eprintln!("No peers found.")
            }
        }
        cli::Command::LoadTest { clients } => {
            let topics = (0u8..9)
                .map(|i| blake3::hash(&i.to_be_bytes()))
                .collect::<Vec<_>>();
            let mut handles = vec![];
            let reqs = Arc::new(AtomicU64::new(0));
            for _i in 0..clients {
                let topics = topics.clone();
                let reqs = reqs.clone();
                let client_fut = async move {
                    let client = quic::connect(tracker_addr).await?;
                    let command = proto::Command::ProvideTopic(proto::ProvideTopic {
                        topic: *topics.choose(&mut rand::rngs::OsRng {}).unwrap().as_bytes(), // topic: *topics[i].as_bytes(),
                    });
                    client.request_one(command).await?;
                    for _i in 0.. {
                        let command = proto::Command::LookupTopic(proto::LookupTopic {
                            topic: *topics.choose(&mut rand::rngs::OsRng {}).unwrap().as_bytes(), // topic: *topics[i % 8].as_bytes(),
                        });
                        let _res = client.request_one(command).await?;
                        reqs.fetch_add(1, Ordering::Relaxed);
                    }
                    anyhow::Result::<(), anyhow::Error>::Ok(())
                };
                let handle = tokio::spawn(client_fut);
                handles.push(handle);
            }

            tokio::spawn(async move {
                let start = Instant::now();
                let mut last_now = start.clone();
                let mut last_reqs = 0f64;
                loop {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    let now = Instant::now();
                    let reqs = reqs.load(Ordering::Relaxed) as f64;
                    println!(
                        "last reqs/s {:.2} total req/s {:.2}",
                        (reqs - last_reqs) / last_now.elapsed().as_secs_f64(),
                        (reqs / start.elapsed().as_secs_f64())
                    );
                    last_now = now;
                    last_reqs = reqs;
                }
            });
            // stop on the first error.
            select_all(handles).await.0.unwrap().unwrap();
        }
    }
    Ok(())
}
