//! Fuzz the handshake and the calls of `zoe serve`.
//!
//! One server runs in the process, on a new store in the temporary
//! directory. It listens on a unix domain socket. A TCP connection that
//! closes keeps its port for a time, and a run makes many more connections
//! than the ports of macOS. An input is the bytes that one client sends:
//! its protocol, then its frames. The target sends them on a new
//! connection, closes its side for writes, and reads until the server
//! closes the connection.
//!
//! A panic in a thread of the server aborts the process, so the run fails.
//! A server that does not close the connection makes the run time out.

#![no_main]

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use zoe_server::serve::{Address, ServeOptions, Server};

/// The server of the run.
static SERVER: LazyLock<Server> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("zoe-fuzz-handshake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the directory of the run is made");
    let socket = Address::Unix(dir.join("zoe.sock"));
    Server::start(ServeOptions::new(dir.join("store")).listen(socket)).expect("the server starts")
});

/// The path of the socket of the server.
static SOCKET: LazyLock<std::path::PathBuf> = LazyLock::new(|| match SERVER.address() {
    Address::Unix(path) => path.clone(),
    Address::Tcp(addr) => panic!("the server listens on TCP {addr}"),
});

fuzz_target!(|data: &[u8]| {
    let stream = UnixStream::connect(&*SOCKET).expect("the server accepts a connection");
    let mut writer = stream.try_clone().expect("the stream clones");
    // Another thread writes, so that replies that fill the socket do not
    // stop the writes. The server can close the connection before it reads
    // all of the input. Then the write fails, and that is not a fault.
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = writer.write_all(data);
            let _ = writer.shutdown(Shutdown::Write);
        });
        let mut replies = Vec::new();
        let _ = (&stream).read_to_end(&mut replies);
    });
});
