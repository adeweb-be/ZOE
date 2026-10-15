//! The server CPU of a load of 1 KB (N4).
//!
//! Clients load records of 1 KB from `zoe serve`, each client on its own
//! connection, one load at a time, as a stock client does. The test reads
//! the CPU time of the server process before and after the loads, from
//! `/proc/<pid>/stat`, and prints the CPU of the server per load and the
//! wall time of one load, for 1 and for 4 clients.
//!
//! The numbers mean something only in a release build:
//!
//! ```text
//! cargo test --release -p zoe-server --test load_cpu -- --ignored --nocapture
//! ```
//!
//! With `LOAD_CPU_BASELINE` set to the path of another `zoe` binary, the
//! test serves the same records from both binaries and takes their samples
//! in turn. So a load of the machine that changes during the run weighs on
//! both rows alike.
//!
//! The test always checks that each load gives the record of its oid.

use std::thread;
use std::time::{Duration, Instant};

mod wire;

use wire::{Conn, MAX, Server, Value, ZOE, bytes_of, commit, fresh_dir, items, new_oids, p64, s};

/// The size of the bytes in the state of each record.
const SIZE: usize = 1000;

/// The objects that the clients load.
const OBJECTS: usize = 1000;

/// The loads of each sample, for all the clients together.
const LOADS: usize = 40_000;

/// The samples of each row. The test prints their median.
const SAMPLES: usize = 7;

/// Return a record of a `PersistentMapping` whose state holds `SIZE`
/// bytes under the key `v`. The bytes come from `seed`.
fn record_1k(seed: u64) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    pickle.extend_from_slice(&u32::try_from(SIZE).unwrap().to_le_bytes());
    let mut state = seed | 1;
    pickle.extend((0..SIZE).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state as u8
    }));
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

/// The clock ticks of `/proc` in one second.
fn clock_ticks() -> u64 {
    let output = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .unwrap();
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

/// Load `loads` records through `conn`, one at a time, and check each.
fn load_all(conn: &mut Conn, oids: &[u64], start: usize, loads: usize) {
    for n in 0..loads {
        let oid = oids[(start + n) % oids.len()];
        let reply = conn
            .call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())])
            .unwrap();
        assert_eq!(bytes_of(&items(&reply)[0]), record_1k(oid));
    }
}

/// A `zoe serve` of one binary, with the records, and the connections of
/// its clients.
struct Served {
    name: &'static str,
    server: Server,
    oids: Vec<u64>,
    /// The connections of 1 client, then of 4 clients.
    rows: Vec<Vec<Conn>>,
    /// The samples of each row: the CPU of the server per load, and the
    /// wall time per load of one client.
    samples: Vec<Vec<(Duration, Duration)>>,
}

impl Served {
    /// Start `binary` on a new store, commit the records, and open the
    /// connections.
    fn start(name: &'static str, binary: &str) -> Served {
        let dir = fresh_dir(&format!("load-cpu-{name}"));
        let store = dir.join("store");
        let server = Server::start_binary(binary, &["--path", s(&store)], &[]);
        let mut writer = server.client("1");
        let mut oids = Vec::new();
        // A call of `new_oids` gives at most 100 oids.
        while oids.len() < OBJECTS {
            oids.extend(new_oids(&mut writer, 100));
        }
        let stores: Vec<_> = oids.iter().map(|oid| (*oid, 0, record_1k(*oid))).collect();
        commit(&mut writer, 1, &stores).unwrap();
        let mut rows: Vec<Vec<Conn>> = [1, 4]
            .iter()
            .map(|clients| (0..*clients).map(|_| server.client("1")).collect())
            .collect();
        // The first pass warms the caches of the store and of the kernel.
        load_all(&mut rows[0][0], &oids, 0, oids.len());
        Served {
            name,
            server,
            oids,
            samples: vec![Vec::new(); rows.len()],
            rows,
        }
    }

    /// Take one sample of the row `row`: its clients load `LOADS` records
    /// together.
    fn sample(&mut self, row: usize) {
        let conns = &mut self.rows[row];
        let oids = &self.oids;
        let clients = conns.len();
        let each = LOADS / clients;
        let before = cpu(self.server.pid());
        let started = Instant::now();
        thread::scope(|scope| {
            for (n, conn) in conns.iter_mut().enumerate() {
                scope.spawn(move || load_all(conn, oids, n * 97, each));
            }
        });
        let wall = started.elapsed();
        let used = cpu(self.server.pid()) - before;
        let loads = u32::try_from(each * clients).unwrap();
        self.samples[row].push((used / loads, wall / u32::try_from(each).unwrap()));
    }

    /// Print the median of each row.
    fn print(&self) {
        for (row, samples) in self.samples.iter().enumerate() {
            let median = |pick: fn(&(Duration, Duration)) -> Duration| {
                let mut values: Vec<Duration> = samples.iter().map(pick).collect();
                values.sort();
                values[values.len() / 2].as_secs_f64() * 1e6
            };
            println!(
                "{}, clients {}: server CPU per load {:.1} µs, wall time per load {:.1} µs (median of {SAMPLES} samples of {LOADS} loads)",
                self.name,
                self.rows[row].len(),
                median(|sample| sample.0),
                median(|sample| sample.1),
            );
        }
    }
}

/// Print the server CPU per load and the wall time per load for 1 and 4
/// clients, of this build and of the baseline binary when it is given.
#[test]
#[ignore = "a measurement: run it in a release build, see the doc of the module"]
fn the_server_cpu_of_a_load_of_1_kb() {
    let mut served = vec![Served::start("this build", ZOE)];
    if let Ok(baseline) = std::env::var("LOAD_CPU_BASELINE") {
        served.push(Served::start("baseline", &baseline));
    }
    for _ in 0..SAMPLES {
        for row in 0..2 {
            for one in &mut served {
                one.sample(row);
            }
        }
    }
    for one in &served {
        one.print();
    }
}
