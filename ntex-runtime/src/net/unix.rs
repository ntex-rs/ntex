use std::{future::Future, io};

use ntex_iodriver::impl_raw_fd;
use socket2::{SockAddr, Socket as Socket2};

use crate::net::Socket;

/// A Unix stream between two local sockets on Windows & WSL.
///
/// A Unix stream can either be created by connecting to an endpoint, via the
/// `connect` method.
#[derive(Debug)]
pub struct UnixStream {
    inner: Socket,
}

impl UnixStream {
    #[cfg(unix)]
    /// Creates new UnixStream from a std::os::unix::net::UnixStream.
    pub fn from_std(stream: std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self {
            inner: Socket::from_socket2(Socket2::from(stream))?,
        })
    }

    /// Creates new TcpStream from a Socket.
    pub fn from_socket(inner: Socket) -> Self {
        Self { inner }
    }

    /// Close the socket. If the returned future is dropped before polling, the
    /// socket won't be closed.
    pub fn close(self) -> impl Future<Output = io::Result<()>> {
        self.inner.close()
    }

    /// Returns the socket path of the remote peer of this connection.
    pub fn peer_addr(&self) -> io::Result<SockAddr> {
        #[allow(unused_mut)]
        let mut addr = self.inner.peer_addr()?;
        #[cfg(windows)]
        {
            fix_unix_socket_length(&mut addr);
        }
        Ok(addr)
    }

    /// Returns the socket path of the local half of this connection.
    pub fn local_addr(&self) -> io::Result<SockAddr> {
        self.inner.local_addr()
    }
}

impl_raw_fd!(UnixStream, socket2::Socket, inner, socket);

#[cfg(windows)]
#[inline]
fn empty_unix_socket() -> SockAddr {
    use windows_sys::Win32::Networking::WinSock::{AF_UNIX, SOCKADDR_UN};

    // SAFETY: the length is correct
    unsafe {
        SockAddr::try_init(|addr, len| {
            let addr: *mut SOCKADDR_UN = addr.cast();
            std::ptr::write(
                addr,
                SOCKADDR_UN {
                    sun_family: AF_UNIX,
                    sun_path: [0; 108],
                },
            );
            std::ptr::write(len, 3);
            Ok(())
        })
    }
    // it is always Ok
    .unwrap()
    .1
}

// The peer addr returned after ConnectEx is buggy. It contains bytes that
// should not belong to the address. Luckily a unix path should not contain `\0`
// until the end. We can determine the path ending by that.
#[cfg(windows)]
#[inline]
fn fix_unix_socket_length(addr: &mut SockAddr) {
    use windows_sys::Win32::Networking::WinSock::SOCKADDR_UN;

    // SAFETY: cannot construct non-unix socket address in safe way.
    let unix_addr: &SOCKADDR_UN = unsafe { &*addr.as_ptr().cast() };
    let addr_len = match std::ffi::CStr::from_bytes_until_nul(&unix_addr.sun_path) {
        Ok(str) => str.to_bytes_with_nul().len() + 2,
        Err(_) => std::mem::size_of::<SOCKADDR_UN>(),
    };
    unsafe {
        addr.set_length(addr_len as _);
    }
}
