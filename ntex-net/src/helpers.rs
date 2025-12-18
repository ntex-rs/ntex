use std::{io, net, net::SocketAddr, path::Path};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

pub(crate) async fn connect(addr: SocketAddr) -> io::Result<Socket> {
    let addr = SockAddr::from(addr);
    let domain = addr.domain();
    connect_inner(addr, domain, Type::STREAM, Some(Protocol::TCP)).await
}

pub(crate) async fn connect_unix(path: impl AsRef<Path>) -> io::Result<Socket> {
    let addr = SockAddr::unix(path)?;
    connect_inner(addr, Domain::UNIX, Type::STREAM, None).await
}

async fn connect_inner(
    addr: SockAddr,
    domain: Domain,
    ty: Type,
    protocol: Option<Protocol>,
) -> io::Result<Socket> {
    let sock = prep_socket(Socket::new(domain, ty, protocol)?)?;
    crate::rt_impl::connect::ConnectOps::current()
        .connect(sock, addr)
        .await
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

pub(crate) fn close_socket(sock: Socket) {
    ntex_rt::spawn_blocking(move || {
        let _ = sock.shutdown(net::Shutdown::Both);
        drop(sock);
    });
}
