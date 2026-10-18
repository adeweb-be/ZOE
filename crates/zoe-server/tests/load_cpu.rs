//! The server CPU of a load (N4): records of 1 KB, and records of 100 KB.
//!
//! Clients load records from `zoe serve`, each client on its own
//! connection, one load at a time, as a stock client does. The test reads
//! the CPU time of the server process before and after the loads, from
//! `/proc/<pid>/stat`, and prints the CPU of the server per load and the
//! wall time of one load, for 1 and for 4 clients. It also prints the mean
//! of `zoe_load_latency_seconds` of `/metrics`: the time of the call of
//! each load in the server, from its start until just before its reply
//! goes into the queue of the connection.
//!
//! The records of 100 KB are of two kinds. The text records hold words of
//! a list, as the records of the harness do, so zstd compresses them. The
//! random records hold bytes that do not compress. For these, the test
//! also prints the time of `Store::load_before` in the test process, on a
//! store with the same records: the part of the core in a load.
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
//! The tests always check that each load gives the record of its oid.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

mod wire;

use wire::{Conn, MAX, Server, Value, ZOE, bytes_of, commit, fresh_dir, items, new_oids, p64, s};

/// The samples of each row. The test prints their median.
const SAMPLES: usize = 7;

/// The records of one measurement.
#[derive(Clone, Copy)]
struct Records {
    /// The name of the records in the lines that the test prints.
    name: &'static str,
    /// The size of the bytes in the state of each record.
    size: usize,
    /// True when the bytes are words, false when they do not compress.
    text: bool,
    /// The objects that the clients load.
    objects: usize,
    /// The loads of each sample, for all the clients together.
    loads: usize,
}

const SMALL: Records = Records {
    name: "1 KB",
    size: 1000,
    text: false,
    objects: 1000,
    loads: 40_000,
};

const TEXT: Records = Records {
    name: "100 KB text",
    size: 100_000,
    text: true,
    objects: 200,
    loads: 6_000,
};

const RANDOM: Records = Records {
    name: "100 KB random",
    size: 100_000,
    text: false,
    objects: 200,
    loads: 6_000,
};

/// Return `len` bytes that come from `seed`: words of a list of 2,000
/// words when `text` is true, else bytes that do not compress.
fn bytes(seed: u64, len: usize, text: bool) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    if !text {
        return (0..len).map(|_| next() as u8).collect();
    }
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        // A word is the number of its place in the list, spelled with the
        // letters a to p.
        let mut letters = next() % 2000;
        loop {
            out.push(b'a' + (letters % 16) as u8);
            letters /= 16;
            if letters == 0 {
                break;
            }
        }
        out.push(b' ');
    }
    out.truncate(len);
    out
}

/// Return the record of `oid`: a `PersistentMapping` whose state holds the
/// bytes of `records` under the key `v`.
fn record(records: Records, oid: u64) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    pickle.extend_from_slice(&u32::try_from(records.size).unwrap().to_le_bytes());
    pickle.extend(bytes(oid, records.size, records.text));
    pickle.extend_from_slice(b"q\x02s.");
    pickle
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

/// The sum in seconds and the count of `zoe_load_latency_seconds` of the
/// admin port at `admin`.
fn load_latency(admin: SocketAddr) -> (f64, f64) {
    let mut stream = TcpStream::connect(admin).unwrap();
    write!(
        stream,
        "GET /metrics HTTP/1.1\r\nHost: zoe\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let value = |name: &str| {
        response
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.rsplit_once(' '))
            .map_or(0.0, |(_, value)| value.parse().unwrap())
    };
    (
        value("zoe_load_latency_seconds_sum{"),
        value("zoe_load_latency_seconds_count{"),
    )
}

/// Load `loads` records through `conn`, one at a time, and check each.
fn load_all(conn: &mut Conn, records: Records, oids: &[u64], start: usize, loads: usize) {
    for n in 0..loads {
        let oid = oids[(start + n) % oids.len()];
        let reply = conn
            .call("loadBefore", vec![p64(oid), Value::Binary(MAX.to_vec())])
            .unwrap();
        assert_eq!(bytes_of(&items(&reply)[0]), record(records, oid));
    }
}

/// One sample: the CPU of the server per load, the wall time per load of
/// one client, and the mean time of the call of a load in the server.
type Sample = (Duration, Duration, Duration);

/// A `zoe serve` of one binary, with the records, and the connections of
/// its clients.
struct Served {
    name: &'static str,
    records: Records,
    server: Server,
    oids: Vec<u64>,
    /// The connections of 1 client, then of 4 clients.
    rows: Vec<Vec<Conn>>,
    /// The samples of each row.
    samples: Vec<Vec<Sample>>,
}

impl Served {
    /// Start `binary` on a new store, commit the records, and open the
    /// connections.
    fn start(name: &'static str, binary: &str, records: Records) -> Served {
        let dir = fresh_dir(&format!(
            "load-cpu-{}-{}",
            name.replace(' ', "-"),
            records.name.replace(' ', "-")
        ));
        let store = dir.join("store");
        let args = ["--path", s(&store), "--admin-listen", "127.0.0.1:0"];
        let server = Server::start_binary(binary, &args, &[]);
        let mut writer = server.client("1");
        let mut oids = Vec::new();
        // A call of `new_oids` gives at most 100 oids.
        while oids.len() < records.objects {
            oids.extend(new_oids(&mut writer, 100));
        }
        oids.truncate(records.objects);
        for (n, chunk) in oids.chunks(100).enumerate() {
            let stores: Vec<_> = chunk
                .iter()
                .map(|oid| (*oid, 0, record(records, *oid)))
                .collect();
            commit(&mut writer, i64::try_from(n + 1).unwrap(), &stores).unwrap();
        }
        let mut rows: Vec<Vec<Conn>> = [1, 4]
            .iter()
            .map(|clients| (0..*clients).map(|_| server.client("1")).collect())
            .collect();
        // The first pass warms the caches of the store and of the kernel.
        load_all(&mut rows[0][0], records, &oids, 0, oids.len());
        Served {
            name,
            records,
            server,
            oids,
            samples: vec![Vec::new(); rows.len()],
            rows,
        }
    }

    /// Take one sample of the row `row`: its clients load the records of
    /// one sample together.
    fn sample(&mut self, row: usize) {
        let records = self.records;
        let conns = &mut self.rows[row];
        let oids = &self.oids;
        let clients = conns.len();
        let each = records.loads / clients;
        let admin = self.server.admin.unwrap();
        let latency = load_latency(admin);
        let before = cpu(self.server.pid());
        let started = Instant::now();
        thread::scope(|scope| {
            for (n, conn) in conns.iter_mut().enumerate() {
                scope.spawn(move || load_all(conn, records, oids, n * 97, each));
            }
        });
        let wall = started.elapsed();
        let used = cpu(self.server.pid()) - before;
        let after = load_latency(admin);
        let loads = u32::try_from(each * clients).unwrap();
        let call = Duration::from_secs_f64((after.0 - latency.0) / (after.1 - latency.1));
        self.samples[row].push((used / loads, wall / u32::try_from(each).unwrap(), call));
    }

    /// Print the median of each row.
    fn print(&self) {
        for (row, samples) in self.samples.iter().enumerate() {
            let median = |pick: fn(&Sample) -> Duration| {
                let mut values: Vec<Duration> = samples.iter().map(pick).collect();
                values.sort();
                values[values.len() / 2].as_secs_f64() * 1e6
            };
            println!(
                "{}, {}, clients {}: server CPU per load {:.1} µs, wall time per load {:.1} µs, call of a load in the server {:.1} µs (median of {SAMPLES} samples of {} loads)",
                self.records.name,
                self.name,
                self.rows[row].len(),
                median(|sample| sample.0),
                median(|sample| sample.1),
                median(|sample| sample.2),
                self.records.loads,
            );
        }
    }
}

/// Print the time of `Store::load_before` in this process, on a store
/// with the records of `records`: the median of 7 passes over the records.
fn print_core(records: Records) {
    use zoe_core::{Oid, Tid};

    let dir = fresh_dir(&format!("load-cpu-core-{}", records.name.replace(' ', "-")));
    let store = wire::open_store(&dir.join("store"));
    let oids: Vec<u64> = (1..=u64::try_from(records.objects).unwrap()).collect();
    let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
    for oid in &oids {
        txn.store(Oid(*oid), Tid(0), &record(records, *oid))
            .unwrap();
    }
    txn.vote().unwrap();
    txn.finish().unwrap();
    let len = record(records, 1).len();
    let mut passes: Vec<Duration> = (0..8)
        .map(|_| {
            let started = Instant::now();
            for oid in &oids {
                let loaded = store.load_before(Oid(*oid), Tid::MAX).unwrap().unwrap();
                assert_eq!(loaded.data.len(), len);
            }
            started.elapsed() / u32::try_from(oids.len()).unwrap()
        })
        .collect();
    // The first pass warms the caches.
    passes.remove(0);
    passes.sort();
    println!(
        "{}, Store::load_before in the test process: {:.1} µs",
        records.name,
        passes[passes.len() / 2].as_secs_f64() * 1e6
    );
}

/// Measure the loads of `records` from this build, and from the baseline
/// binary when it is given, and print the numbers.
fn measure(records: Records) {
    let mut served = vec![Served::start("this build", ZOE, records)];
    if let Ok(baseline) = std::env::var("LOAD_CPU_BASELINE") {
        served.push(Served::start("baseline", &baseline, records));
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

/// Print the server CPU per load and the wall time per load of a record
/// of 1 KB.
#[test]
#[ignore = "a measurement: run it in a release build, see the doc of the module"]
fn the_server_cpu_of_a_load_of_1_kb() {
    measure(SMALL);
}

/// Print the server CPU per load and the wall time per load of a record
/// of 100 KB, of text and of random bytes, and the time of the core.
#[test]
#[ignore = "a measurement: run it in a release build, see the doc of the module"]
fn the_server_cpu_of_a_load_of_100_kb() {
    for records in [TEXT, RANDOM] {
        print_core(records);
        measure(records);
    }
}
