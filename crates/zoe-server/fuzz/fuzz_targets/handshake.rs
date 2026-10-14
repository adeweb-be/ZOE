//! Fuzz the handshake and the calls of `zoe serve`.
//!
//! One server runs in the process, on a new store in the temporary
//! directory. An input is the bytes that one client sends: its protocol,
//! then its frames. The target sends them on a new connection, closes its
//! side for writes, and reads until the server closes the connection.
//!
//! A panic in a thread of the server aborts the process, so the run fails.
//! A server that does not close the connection makes the run time out.

#![no_main]

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use zoe_server::serve::{ServeOptions, Server};

/// The server of the run.
static SERVER: LazyLock<Server> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("zoe-fuzz-handshake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Server::start(ServeOptions::new(&dir)).expect("the server starts")
});

fuzz_target!(|data: &[u8]| {
    let stream = TcpStream::connect(SERVER.addr()).expect("the server accepts a connection");
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
