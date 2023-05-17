use clap::Parser;
use rand::seq::SliceRandom;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::task::{spawn, JoinHandle};

use waht::io::quic;
use waht::tracker::{
    proto::{self, Command},
    Topic,
};

#[derive(Debug, Parser)]
struct Cli {
    #[clap(short, long, default_value = "4")]
    clients: usize,
    #[clap(short, long, default_value = "4")]
    socket_threads: usize,
    #[clap(short, long, default_value = "3")]
    time: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt::init();
    let opts = Cli::parse();

    let tracker_addr: SocketAddr = "127.0.0.1:39210".parse().unwrap();
    let mut handles: Vec<JoinHandle<anyhow::Result<()>>> = vec![];
    // let config = quic::server::Config {
    //     socket_threads: Some(opts.socket_threads),
    // };
    let handle = spawn(quic::listen(tracker_addr, Default::default()));
    handles.push(handle);

    let topics: Vec<Topic> = (0..opts.clients).map(|_x| rand::random()).collect();
    let mut rng = rand::rngs::OsRng;
    let reqs = Arc::new(AtomicU64::new(0));

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(opts.time)).await;
        std::process::exit(0);
    });

    for i in 0..opts.clients {
        let reqs = reqs.clone();
        let topics = topics.clone();
        let thread = thread::Builder::new().name(format!("waht-client-{i}"));
        thread.spawn(move || {
            let fut = async move {
                let client = quic::connect(tracker_addr).await?;
                let _res = client
                    .request_one(Command::ProvideTopic(proto::ProvideTopic {
                        topic: *topics.choose(&mut rng).unwrap(),
                    }))
                    .await?;
                loop {
                    let _res = client
                        .request_one(Command::LookupTopic(proto::LookupTopic {
                            topic: *topics.choose(&mut rng).unwrap(),
                        }))
                        .await?;
                    reqs.fetch_add(1, Ordering::Relaxed);
                }
            };
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let res: anyhow::Result<()> = rt.block_on(fut);
            res
        }).unwrap();
    }
    let report = tokio::spawn(async move {
        let start = Instant::now();
        let mut last_now = start.clone();
        let mut last_reqs = 0f64;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = Instant::now();
            let reqs = reqs.load(Ordering::Relaxed) as f64;
            println!(
                "reqs/s {:9.2} | avg req/s {:9.2} | total reqs {reqs:12}",
                (reqs - last_reqs) / last_now.elapsed().as_secs_f64(),
                (reqs / start.elapsed().as_secs_f64())
            );
            last_now = now;
            last_reqs = reqs;
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    });
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    report.abort();
}
