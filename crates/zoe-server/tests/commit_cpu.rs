//! The server CPU of a commit of 100 objects (N4).
//!
//! Clients commit transactions of 100 new objects to `zoe serve`, each
//! client on its own connection, one transaction at a time, as a stock
//! client does: `tpc_begin`, 100 `storea` without a reply, `vote`,
//! `tpc_finish`. The test reads the CPU time of the server process before
//! and after the commits, from `/proc/<pid>/stat`, and prints the CPU of
//! the server per object and the wall time of one transaction, for
//! records of 1 KB and of 100 KB, and for 1 and 4 clients. The wall time
//! holds the sync of each finish.
//!
//! The numbers mean something only in a release build:
//!
//! ```text
//! cargo test --release -p zoe-server --test commit_cpu -- --ignored --nocapture
//! ```
//!
//! With `COMMIT_CPU_BASELINE` set to the path of another `zoe` binary, the
//! test commits the same records to both binaries and takes their samples
//! in turn. So a load of the machine that changes during the run weighs
//! on both rows alike. `COMMIT_CPU_DIR` sets the directory of the stores,
//! the temporary directory of cargo by default, which is on the disk.
//!
//! The test always checks that the last object of each sample loads.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

mod wire;

use wire::{Conn, Server, ZOE, commit, load, new_oids, s};

/// The objects of each transaction.
const OBJECTS: usize = 100;

/// The samples of each row. The test prints their median.
const SAMPLES: usize = 5;

/// A record of `size` bytes: the first half is text that zstd compresses
/// well, the second half is bytes that it does not, as in `finish_time`
/// of the core.
fn record(oid: u64, size: usize) -> Vec<u8> {
    let mut data = b"\x80\x03czodbshootout.speedtest\nPObject\nq\x00.\x80\x03}q\x01(".to_vec();
    while data.len() < size / 2 {
        data.extend_from_slice(b"X\x04\x00\x00\x00attrq\x02K\x01X\x04\x00\x00\x00dataq\x03");
    }
    let mut state = oid ^ 0x9e37_79b9_7f4a_7c15;
    while data.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(size);
    data
}

/// The clock ticks of `/proc` in one second.
fn clock_ticks() -> u64 {
    let output = {
        let _spawn = wire::spawn_lock();
        std::process::Command::new("getconf")
            .arg("CLK_TCK")
            .output()
            .unwrap()
    };
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The CPU time of the process `pid`, user and system, its threads that
/// ended included.
fn cpu(pid: u32) -> Duration {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // The name of the command is in parentheses and can hold spaces.
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = rest.split(' ').collect();
    // `utime` and `stime` are the fields 14 and 15 of the line, so 11 and
    // 12 after the state, in clock ticks.
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    Duration::from_nanos(ticks * 1_000_000_000 / clock_ticks())
}

/// The minor page faults of the process `pid`, its threads that ended
/// included.
fn faults(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 2..];
    // `minflt` is the field 10 of the line, so 7 after the state.
    rest.split(' ').nth(7).unwrap().parse().unwrap()
}

/// Commit `txns` transactions of `OBJECTS` new objects of `size` bytes
/// through `conn`. Return the last oid.
fn commit_all(conn: &mut Conn, size: usize, txns: usize) -> u64 {
    let mut last = 0;
    for txn in 0..txns {
        let stores: Vec<_> = new_oids(conn, OBJECTS as u64)
            .into_iter()
            .map(|oid| (oid, 0, record(oid, size)))
            .collect();
        last = stores.last().unwrap().0;
        commit(conn, txn as i64 + 1, &stores).unwrap();
    }
    last
}

/// One row: the size of a record, the clients, and the transactions of
/// each client in a sample.
#[derive(Clone, Copy)]
struct Row {
    size: usize,
    clients: usize,
    txns: usize,
}

const ROWS: [Row; 4] = [
    Row {
        size: 1024,
        clients: 1,
        txns: 40,
    },
    Row {
        size: 1024,
        clients: 4,
        txns: 10,
    },
    Row {
        size: 100 * 1024,
        clients: 1,
        txns: 8,
    },
    Row {
        size: 100 * 1024,
        clients: 4,
        txns: 2,
    },
];

/// A `zoe serve` of one binary and the connections of its clients.
struct Served {
    name: &'static str,
    server: Server,
    conns: Vec<Conn>,
    /// The samples of each row: the CPU of the server per object, and the
    /// wall time per transaction of one client.
    samples: Vec<Vec<(Duration, Duration)>>,
    /// The minor page faults of the server per object, in each sample of
    /// each row.
    faults: Vec<Vec<u64>>,
}

impl Served {
    /// Start `binary` on a new store and open the connections.
    fn start(name: &'static str, binary: &str) -> Served {
        let dir = std::env::var_os("COMMIT_CPU_DIR")
            .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from)
            .join(format!("commit-cpu-{}", name.replace(' ', "-")));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("store");
        let server = Server::start_binary(binary, &["--path", s(&store)], &[]);
        let mut conns: Vec<Conn> = (0..4).map(|_| server.client("1")).collect();
        // The first transactions warm the store and the threads.
        commit_all(&mut conns[0], 1024, 5);
        Served {
            name,
            server,
            conns,
            samples: vec![Vec::new(); ROWS.len()],
            faults: vec![Vec::new(); ROWS.len()],
        }
    }

    /// Take one sample of the row `row`.
    fn sample(&mut self, row: usize) {
        let Row {
            size,
            clients,
            txns,
        } = ROWS[row];
        let before = cpu(self.server.pid());
        let faulted = faults(self.server.pid());
        let started = Instant::now();
        let lasts: Vec<u64> = thread::scope(|scope| {
            let running: Vec<_> = self.conns[..clients]
                .iter_mut()
                .map(|conn| scope.spawn(move || commit_all(conn, size, txns)))
                .collect();
            running.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let wall = started.elapsed();
        let used = cpu(self.server.pid()) - before;
        let faulted = faults(self.server.pid()) - faulted;
        for last in lasts {
            assert_eq!(
                load(&mut self.conns[0], last).unwrap().0,
                record(last, size)
            );
        }
        let objects = u32::try_from(OBJECTS * clients * txns).unwrap();
        let per_txn = wall / u32::try_from(txns).unwrap();
        self.samples[row].push((used / objects, per_txn));
        self.faults[row].push(faulted / u64::from(objects));
    }

    /// Print the median of each row.
    fn print(&self) {
        for (row, samples) in self.samples.iter().enumerate() {
            let median = |pick: fn(&(Duration, Duration)) -> Duration| {
                let mut values: Vec<Duration> = samples.iter().map(pick).collect();
                values.sort();
                values[values.len() / 2].as_secs_f64() * 1e6
            };
            let Row { size, clients, .. } = ROWS[row];
            let mut faults = self.faults[row].clone();
            faults.sort_unstable();
            println!(
                "{}, {size} bytes, clients {clients}: server CPU per object {:.1} µs, wall time per transaction {:.0} µs, minor page faults per object {} (median of {SAMPLES} samples)",
                self.name,
                median(|sample| sample.0),
                median(|sample| sample.1),
                faults[faults.len() / 2],
            );
        }
    }
}

/// Print the server CPU per object and the wall time per transaction of
/// each row, of this build and of the baseline binary when it is given.
#[test]
#[ignore = "a measurement: run it in a release build, see the doc of the module"]
fn the_server_cpu_of_a_commit_of_100_objects() {
    let mut served = vec![Served::start("this build", ZOE)];
    if let Ok(baseline) = std::env::var("COMMIT_CPU_BASELINE") {
        served.push(Served::start("baseline", &baseline));
    }
    for _ in 0..SAMPLES {
        for row in 0..ROWS.len() {
            for one in &mut served {
                one.sample(row);
            }
        }
    }
    for one in &served {
        one.print();
    }
}
