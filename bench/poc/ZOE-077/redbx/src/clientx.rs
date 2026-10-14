//! The client side of ZEO commits and loads, with `zoe-client`. Not for merge.
//!
//! `clientx.py` runs the same workload with `ZEO.ClientStorage`. Each
//! worker commits `--txns` transactions of 100 new objects (`add`), then
//! `--txns` transactions that rewrite its last 100 objects (`update`),
//! then loads each object that it added, on a new connection (`load`).
//! With `--shared`, the workers use one client, as the threads cells of
//! zodbshootout do. Without it, each worker has its own client, as the
//! processes cells do. The report is one JSON line.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use zoe_client::Client;
use zoe_core::{Oid, Tid};

struct Args {
    server: SocketAddr,
    workers: usize,
    shared: bool,
    txns: usize,
    samples: String,
    server_pid: Option<u32>,
    threads: usize,
    label: String,
}

fn parse() -> Args {
    let mut args = Args {
        server: "127.0.0.1:0".parse().unwrap(),
        workers: 16,
        shared: false,
        txns: 20,
        samples: String::new(),
        server_pid: None,
        threads: 4,
        label: "rust".into(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        if flag == "--shared" {
            args.shared = true;
            continue;
        }
        let value = it.next().expect("a value");
        match flag.as_str() {
            "--server" => args.server = value.parse().unwrap(),
            "--workers" => args.workers = value.parse().unwrap(),
            "--txns" => args.txns = value.parse().unwrap(),
            "--samples" => args.samples = value,
            "--server-pid" => args.server_pid = Some(value.parse().unwrap()),
            "--threads" => args.threads = value.parse().unwrap(),
            "--label" => args.label = value,
            _ => panic!("unknown flag {flag}"),
        }
    }
    args
}

/// The CPU time of a process in seconds: utime and stime, in ticks of 100 Hz.
fn cpu(pid: Option<u32>) -> f64 {
    let path = pid.map_or("/proc/self/stat".into(), |p| format!("/proc/{p}/stat"));
    let text = std::fs::read_to_string(path).unwrap();
    let fields: Vec<&str> = text.rsplit_once(')').unwrap().1.split_whitespace().collect();
    (fields[11].parse::<f64>().unwrap() + fields[12].parse::<f64>().unwrap()) / 100.0
}

fn samples(path: &str) -> Vec<Vec<u8>> {
    let raw = std::fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let n = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) as usize;
        out.push(raw[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    out
}

async fn connect(args: &Args) -> Vec<Arc<Client>> {
    let n = if args.shared { 1 } else { args.workers };
    let mut clients = Vec::with_capacity(n);
    for _ in 0..n {
        clients.push(Arc::new(Client::connect(args.server, "1").await.unwrap()));
    }
    clients
}

fn client_of(clients: &[Arc<Client>], worker: usize) -> Arc<Client> {
    clients[worker % clients.len()].clone()
}

/// One phase: run `f` on each worker, and report the wall time and the CPU
/// of the client and of the server.
struct Phase {
    wall: f64,
    client: f64,
    server: f64,
}

async fn phase<T, F, Fut>(args: &Args, f: F) -> (Phase, Vec<T>)
where
    T: Send + 'static,
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    let c0 = cpu(None);
    let s0 = args.server_pid.map_or(0.0, |p| cpu(Some(p)));
    let t0 = Instant::now();
    let handles: Vec<_> = (0..args.workers).map(|w| tokio::spawn(f(w))).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.unwrap());
    }
    let wall = t0.elapsed().as_secs_f64();
    let client = cpu(None) - c0;
    let server = args.server_pid.map_or(0.0, |p| cpu(Some(p))) - s0;
    (Phase { wall, client, server }, out)
}

/// The time of each step of the commits of a worker, as the client sees
/// it: the wait for the commit lock of the client, the stores, the vote and
/// the finish.
#[derive(Default)]
struct Steps {
    commits: u64,
    begin: f64,
    store: f64,
    vote: f64,
    finish: f64,
}

impl Steps {
    fn add(&mut self, t0: Instant, t1: Instant, t2: Instant, t3: Instant) {
        let now = Instant::now();
        self.commits += 1;
        self.begin += (t1 - t0).as_secs_f64();
        self.store += (t2 - t1).as_secs_f64();
        self.vote += (t3 - t2).as_secs_f64();
        self.finish += (now - t3).as_secs_f64();
    }

    fn sum<'a>(all: impl Iterator<Item = &'a Steps>) -> Steps {
        let mut out = Steps::default();
        for s in all {
            out.commits += s.commits;
            out.begin += s.begin;
            out.store += s.store;
            out.vote += s.vote;
            out.finish += s.finish;
        }
        out
    }

    /// The mean of each step in ms, as the keys of an object that the
    /// caller closes.
    fn json(&self) -> String {
        let n = self.commits.max(1) as f64 / 1e3;
        format!(
            ",\"begin_ms\":{:.3},\"store_ms\":{:.3},\"vote_ms\":{:.3},\"finish_ms\":{:.3}}}",
            self.begin / n,
            self.store / n,
            self.vote / n,
            self.finish / n,
        )
    }
}

/// The keys of a phase, as an object that the caller closes.
fn report(p: &Phase, commits: usize, objects: usize) -> String {
    format!(
        "{{\"wall_s\":{:.3},\"commits_per_s\":{:.0},\"objects_per_s\":{:.0},\"client_cpu_s\":{:.2},\"client_us_per_object\":{:.1},\"server_cpu_s\":{:.2},\"server_us_per_object\":{:.1}",
        p.wall,
        commits as f64 / p.wall,
        objects as f64 / p.wall,
        p.client,
        p.client * 1e6 / objects as f64,
        p.server,
        p.server * 1e6 / objects as f64,
    )
}

async fn run(args: Args) {
    let data = Arc::new(samples(&args.samples));
    let txns = args.txns;
    let clients = connect(&args).await;

    let (add, added) = phase(&args, |w| {
        let client = client_of(&clients, w);
        let data = data.clone();
        async move {
            let mut all: Vec<Oid> = Vec::with_capacity(txns * 100);
            let mut last = (Vec::new(), Tid(0));
            let mut steps = Steps::default();
            for t in 0..txns {
                let oids = client.new_oids(100).await.unwrap();
                let t0 = Instant::now();
                let mut txn = client.begin(b"", b"", b"").await.unwrap();
                let t1 = Instant::now();
                for (i, oid) in oids.iter().enumerate() {
                    let sample = &data[(w * 7919 + t * 100 + i) % data.len()];
                    txn.store(*oid, Tid(0), sample).await.unwrap();
                }
                let t2 = Instant::now();
                txn.vote().await.unwrap();
                let t3 = Instant::now();
                let tid = txn.finish().await.unwrap();
                steps.add(t0, t1, t2, t3);
                all.extend(&oids);
                last = (oids, tid);
            }
            (all, last, steps)
        }
    })
    .await;
    let add_steps = Steps::sum(added.iter().map(|(_, _, s)| s));
    let added: Vec<_> = added.into_iter().map(|(all, last, _)| (all, last)).collect();

    let lasts: Vec<_> = added.iter().map(|(_, last)| last.clone()).collect();
    let (update, updated) = phase(&args, |w| {
        let client = client_of(&clients, w);
        let data = data.clone();
        let (oids, mut serial) = lasts[w].clone();
        async move {
            let mut steps = Steps::default();
            for t in 0..txns {
                let t0 = Instant::now();
                let mut txn = client.begin(b"", b"", b"").await.unwrap();
                let t1 = Instant::now();
                for (i, oid) in oids.iter().enumerate() {
                    let sample = &data[(w * 104_729 + t * 100 + i) % data.len()];
                    txn.store(*oid, serial, sample).await.unwrap();
                }
                let t2 = Instant::now();
                txn.vote().await.unwrap();
                let t3 = Instant::now();
                serial = txn.finish().await.unwrap();
                steps.add(t0, t1, t2, t3);
            }
            steps
        }
    })
    .await;
    let update_steps = Steps::sum(updated.iter());

    let fresh = connect(&args).await;
    let (load, _) = phase(&args, |w| {
        let client = client_of(&fresh, w);
        let oids = added[w].0.clone();
        async move {
            for oid in oids {
                client.load(oid).await.unwrap();
            }
        }
    })
    .await;

    let commits = args.workers * txns;
    println!(
        "{{\"client\":\"zoe-client\",\"label\":\"{}\",\"workers\":{},\"shared\":{},\"txns\":{},\"add\":{},\"update\":{},\"load\":{}}}",
        args.label,
        args.workers,
        args.shared,
        txns,
        report(&add, commits, commits * 100) + &add_steps.json(),
        report(&update, commits, commits * 100) + &update_steps.json(),
        report(&load, 0, commits * 100) + "}",
    );
}

fn main() {
    let args = parse();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(args.threads)
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(args));
}
