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
        pub tracker: Vec<String>,
    }

    #[derive(Debug, Parser)]
    pub enum Command {
        Server {
            #[clap(short, long, default_value = "0.0.0.0:4399")]
            bind_addr: SocketAddr,
        },
        Provide {
            key: String,
        },
        Lookup {
            key: String,
        },
        LoadTest {
            clients: usize,
            parallel_requests: usize,
        },
    }
}

const DEFAULT_TRACKER: &str = "quic://localhost:4399";

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = cli::Args::parse();

    #[cfg(feature = "thread_per_core")]
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    #[cfg(not(feature = "thread_per_core"))]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(run(cli))?;
    Ok(())
}

async fn run(cli: cli::Args) -> anyhow::Result<()> {
    let tracker_addr = cli
        .tracker
        .into_iter()
        .next()
        .unwrap_or_else(|| DEFAULT_TRACKER.to_owned());
    let client_config = quic::client::Config::accept_insecure();
    match cli.command {
        cli::Command::Server { bind_addr } => {
            let config = quic::server::Config::default();
            quic::listen(bind_addr, config).await?;
        }
        cli::Command::Provide { key } => {
            let key = blake3::hash(key.as_bytes()).into();
            let command = proto::Command::ProvideKey(proto::ProvideKey { key, topic: None });
            let client = quic::connect(&tracker_addr, client_config).await?;
            let res = client.request_one(command).await?;
            eprintln!("{res:?}");
        }
        cli::Command::Lookup { key } => {
            let key = blake3::hash(key.as_bytes()).into();
            let command = proto::Command::LookupKey(proto::LookupKey { key, topic: None });
            let client = quic::connect(&tracker_addr, client_config).await?;
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
        cli::Command::LoadTest {
            clients,
            parallel_requests,
        } => {
            struct State {
                reqs: AtomicU64,
                time_total: AtomicU64,
                time_min: AtomicU64,
                time_max: AtomicU64,
                keys: Vec<blake3::Hash>,
            }
            impl State {
                fn new(num_keys: usize) -> Arc<Self> {
                    let keys = (0..num_keys)
                        .map(|i| blake3::hash(&i.to_be_bytes()))
                        .collect::<Vec<_>>();
                    Arc::new(State {
                        reqs: Default::default(),
                        time_total: Default::default(),
                        time_min: u64::MAX.into(),
                        time_max: Default::default(),
                        keys,
                    })
                }
                fn complete(&self, time: Duration) {
                    self.time_max
                        .fetch_max(time.as_micros() as u64, Ordering::Relaxed);
                    self.time_min
                        .fetch_min(time.as_micros() as u64, Ordering::Relaxed);
                    self.time_total
                        .fetch_add(time.as_micros() as u64, Ordering::Relaxed);
                    self.reqs.fetch_add(1, Ordering::Relaxed);
                }
                fn choose_key(&self) -> &blake3::Hash {
                    self.keys.choose(&mut rand::rngs::OsRng {}).unwrap()
                }
            }

            let state = State::new(10);
            let mut handles = vec![];
            for _i in 0..clients {
                let state = state.clone();
                let client_config = client_config.clone();
                let tracker_addr = tracker_addr.clone();
                let client_fut = async move {
                    let client = quic::connect(&tracker_addr, client_config).await.unwrap();
                    let command = proto::Command::ProvideKey(proto::ProvideKey {
                        key: *state.choose_key().as_bytes(),
                        topic: None,
                    });
                    let _res = client.request_one(command).await.unwrap();
                    let semaphore = Arc::new(tokio::sync::Semaphore::new(parallel_requests));
                    let mut handles = vec![];
                    for _i in 0..parallel_requests {
                        let client = client.clone();
                        let state = state.clone();
                        let semaphore = semaphore.clone();
                        let handle = tokio::spawn(async move {
                            loop {
                                let permit = semaphore.acquire().await.unwrap();
                                let command = proto::Command::LookupKey(proto::LookupKey {
                                    key: *state.choose_key().as_bytes(),
                                    topic: None,
                                });
                                let start = Instant::now();
                                let _res = client.request_one(command).await.unwrap();
                                state.complete(start.elapsed());
                                drop(permit);
                            }
                        });
                        handles.push(handle);
                    }
                    for handle in handles {
                        handle.await.unwrap();
                    }
                    anyhow::Result::<(), anyhow::Error>::Ok(())
                };
                #[cfg(feature = "thread_per_core")]
                let handle = waht::util::spawn_worker_task(|| client_fut);
                #[cfg(not(feature = "thread_per_core"))]
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
                    let reqs = state.reqs.load(Ordering::Relaxed) as f64;
                    let times = state.time_total.load(Ordering::Relaxed) as f64;
                    println!(
                        "reqs/s {:10.2} | total req/s {:10.2} | mean {:6.2}µs | min {:4}µs | max {:4}µs",
                        (reqs - last_reqs) / last_now.elapsed().as_secs_f64(),
                        (reqs / start.elapsed().as_secs_f64()),
                        (times / reqs),
                        state.time_min.load(Ordering::Relaxed),
                        state.time_max.load(Ordering::Relaxed),
                    );
                    last_now = now;
                    last_reqs = reqs;
                }
            });
            // stop on the first error.
            let res = select_all(handles).await.0;
            println!("res {res:?}");
            res.unwrap().unwrap();
        }
    }
    Ok(())
}
