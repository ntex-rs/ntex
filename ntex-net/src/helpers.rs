use std::{io, net::SocketAddr, os::fd::FromRawFd, path::Path};

use ntex_neon::syscall;
use ntex_util::channel::oneshot::channel;
use socket2::{Protocol, SockAddr, Socket, Type};

pub(crate) fn pool_io_err<T, E>(result: std::result::Result<T, E>) -> io::Result<T> {
    result.map_err(|_| io::Error::other("Thread pool panic"))
}

pub(crate) async fn connect(addr: SocketAddr) -> io::Result<Socket> {
    let addr = SockAddr::from(addr);
    let domain = addr.domain().into();
    connect_inner(addr, domain, Type::STREAM.into(), Protocol::TCP.into()).await
}

pub(crate) async fn connect_unix(path: impl AsRef<Path>) -> io::Result<Socket> {
    let addr = SockAddr::unix(path)?;
    connect_inner(addr, socket2::Domain::UNIX.into(), Type::STREAM.into(), 0).await
}

async fn connect_inner(
    addr: SockAddr,
    domain: i32,
    socket_type: i32,
    protocol: i32,
) -> io::Result<Socket> {
    #[allow(unused_mut)]
    let mut ty = socket_type;
    #[cfg(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
    ))]
    {
        ty |= libc::SOCK_CLOEXEC;
    }

    let fd = ntex_rt::spawn_blocking(move || syscall!(libc::socket(domain, ty, protocol)))
        .await
        .map_err(io::Error::other)
        .and_then(pool_io_err)?;

    let (sender, rx) = channel();

    crate::rt_impl::connect::ConnectOps::current().connect(fd, addr, sender)?;

    rx.await
        .map_err(|_| io::Error::other("IO Driver is gone"))
        .and_then(|item| item)?;

    Ok(unsafe { Socket::from_raw_fd(fd) })
}

pub(crate) fn prep_socket(sock: Socket) -> io::Result<Socket> {
    #[cfg(not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "espidf",
        target_os = "vita",
    )))]
    sock.set_cloexec(true)?;
    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos",
    ))]
    sock.set_nosigpipe(true)?;
    sock.set_nonblocking(true)?;

    Ok(sock)
}
