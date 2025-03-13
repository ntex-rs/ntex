use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::{cell::RefCell, io, path::Path, rc::Rc};

use io_uring::{opcode, types::Fd};
use ntex_neon::driver::op::Handler;
use ntex_neon::driver::{AsRawFd, DriverApi, RawFd};
use ntex_neon::net::{Socket, TcpStream, UnixStream};
use ntex_neon::Runtime;
use ntex_util::channel::oneshot::{channel, Sender};
use slab::Slab;
use socket2::{Protocol, SockAddr, Type};

pub(crate) async fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let addr = SockAddr::from(addr);
    let socket = if cfg!(windows) {
        let bind_addr = if addr.is_ipv4() {
            SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        } else if addr.is_ipv6() {
            SockAddr::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0))
        } else {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Unsupported address domain.",
            ));
        };
        Socket::bind(&bind_addr, Type::STREAM, Some(Protocol::TCP)).await?
    } else {
        Socket::new(addr.domain(), Type::STREAM, Some(Protocol::TCP)).await?
    };

    let (sender, rx) = channel();

    ConnectOps::current().connect(socket.as_raw_fd(), addr, sender);

    rx.await
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "IO Driver is gone"))
        .and_then(|item| item)?;

    Ok(TcpStream::from_socket(socket))
}

pub(crate) async fn connect_unix(path: impl AsRef<Path>) -> io::Result<UnixStream> {
    let addr = SockAddr::unix(path)?;

    #[cfg(windows)]
    let socket = {
        let new_addr = empty_unix_socket();
        Socket::bind(&new_addr, Type::STREAM, None).await?
    };
    #[cfg(unix)]
    let socket = {
        use socket2::Domain;
        Socket::new(Domain::UNIX, Type::STREAM, None).await?
    };

    let (sender, rx) = channel();

    ConnectOps::current().connect(socket.as_raw_fd(), addr, sender);

    rx.await
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "IO Driver is gone"))
        .and_then(|item| item)?;

    Ok(UnixStream::from_socket(socket))
}

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

#[derive(Debug)]
enum Change {
    Readable,
    Writable,
    Error(io::Error),
}

struct ConnectOpsHandler {
    inner: Rc<ConnectOpsInner>,
}

struct ConnectOpsInner {
    api: DriverApi,
    ops: RefCell<Slab<Sender<io::Result<()>>>>,
}

impl ConnectOps {
    pub(crate) fn current() -> Self {
        Runtime::with_current(|rt| {
            if let Some(s) = rt.get::<Self>() {
                s
            } else {
                let mut inner = None;
                rt.driver().register(|api| {
                    let ops = Rc::new(ConnectOpsInner {
                        api,
                        ops: RefCell::new(Slab::new()),
                    });
                    inner = Some(ops.clone());
                    Box::new(ConnectOpsHandler { inner: ops })
                });

                let s = ConnectOps(inner.unwrap());
                rt.insert(s.clone());
                s
            }
        })
    }

    pub(crate) fn connect(
        &self,
        fd: RawFd,
        addr: SockAddr,
        sender: Sender<io::Result<()>>,
    ) -> usize {
        let id = self.0.ops.borrow_mut().insert(sender);
        self.0.api.submit(
            id as u32,
            opcode::Connect::new(Fd(fd), addr.as_ptr(), addr.len()).build(),
        );

        id
    }
}

impl Handler for ConnectOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        log::debug!("Op is canceled {:?}", user_data);

        self.inner.ops.borrow_mut().remove(user_data);
    }

    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>) {
        log::debug!("Op is completed {:?} result: {:?}", user_data, result);

        let tx = self.inner.ops.borrow_mut().remove(user_data);
        let _ = tx.send(result.map(|_| ()));
    }
}
