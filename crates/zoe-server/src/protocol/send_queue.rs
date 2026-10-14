//! The send queue of a TCP or unix domain socket.

use std::io;
use std::os::fd::RawFd;

/// Return the count of bytes in the send queue of the socket `fd`. For a
/// TCP socket, these are the bytes that the kernel did not send yet and
/// the bytes that the peer did not acknowledge yet. The FIN counts too.
/// For a unix domain socket, it is the memory of the buffers that the peer
/// did not read yet. Linux gives this count for the ioctl `SIOCOUTQ`, which has
/// the value of `TIOCOUTQ`. The count is 0 when the peer has every byte
/// and, on TCP, the FIN. `fd` must be the descriptor of a TCP or unix
/// domain socket that the caller holds open.
#[allow(unsafe_code)]
pub(crate) fn unacked(fd: RawFd) -> io::Result<usize> {
    let mut count: libc::c_int = 0;
    // SAFETY: The caller holds the stream of `fd` open for the whole call,
    // so `fd` is a live TCP or unix domain socket. For both, the ioctl
    // `TIOCOUTQ` writes one `c_int` through its third argument and reads
    // no other memory. The pointer points to `count`, a local `c_int` that
    // lives until the call returns.
    let result = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &raw mut count) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    usize::try_from(count).map_err(|_| io::Error::other("a negative send queue"))
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;

    use super::*;

    #[test]
    fn a_socket_that_sent_nothing_has_an_empty_queue() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        assert_eq!(unacked(server.as_raw_fd()).unwrap(), 0);
        assert_eq!(unacked(client.as_raw_fd()).unwrap(), 0);
    }

    #[test]
    fn a_unix_socket_counts_the_bytes_that_its_peer_did_not_read() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;

        let (mut server, mut client) = UnixStream::pair().unwrap();
        assert_eq!(unacked(server.as_raw_fd()).unwrap(), 0);
        server.write_all(b"frame").unwrap();
        assert!(unacked(server.as_raw_fd()).unwrap() > 0);
        let mut read = [0u8; 5];
        client.read_exact(&mut read).unwrap();
        assert_eq!(unacked(server.as_raw_fd()).unwrap(), 0);
    }

    #[test]
    fn dev_null_gives_an_error() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(unacked(file.as_raw_fd()).is_err());
    }
}
