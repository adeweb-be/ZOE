//! The send queue of a TCP socket.

use std::io;
use std::os::fd::RawFd;

/// Return the count of bytes in the send queue of the TCP socket `fd`:
/// the bytes that the peer did not acknowledge, the FIN included. Linux
/// gives this count for the ioctl `SIOCOUTQ`, which has the value of
/// `TIOCOUTQ`. A descriptor that is not a TCP socket gives an error.
#[allow(unsafe_code)]
pub(crate) fn unsent(fd: RawFd) -> io::Result<usize> {
    let mut count: libc::c_int = 0;
    // SAFETY: The ioctl `TIOCOUTQ` writes one `c_int` through its third
    // argument and reads no other memory. The pointer points to `count`, a
    // local `c_int` that lives until the call returns. The descriptor is
    // not memory: a closed or wrong descriptor gives an error, never
    // undefined behaviour. The caller holds the stream of `fd`, so the
    // descriptor stays open while the call runs.
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
        assert_eq!(unsent(server.as_raw_fd()).unwrap(), 0);
        assert_eq!(unsent(client.as_raw_fd()).unwrap(), 0);
    }

    #[test]
    fn a_descriptor_that_is_not_a_socket_gives_an_error() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(unsent(file.as_raw_fd()).is_err());
    }
}
