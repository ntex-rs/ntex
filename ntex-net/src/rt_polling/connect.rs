use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::{cell::RefCell, collections::VecDeque, io, path::Path, rc::Rc, task::Poll};

use ntex_neon::driver::op::{Handler, Interest};
use ntex_neon::driver::{AsRawFd, DriverApi, RawFd};
use ntex_neon::net::{Socket, TcpStream, UnixStream};
use ntex_neon::{syscall, Runtime};
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

    ConnectOps::current().connect(socket.as_raw_fd(), addr, sender)?;

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

    ConnectOps::current().connect(socket.as_raw_fd(), addr, sender)?;

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

struct ConnectOpsBatcher {
    feed: VecDeque<(usize, Change)>,
    inner: Rc<ConnectOpsInner>,
}

struct Item {
    fd: RawFd,
    sender: Sender<io::Result<()>>,
}

struct ConnectOpsInner {
    api: DriverApi,
    connects: RefCell<Slab<Item>>,
}

impl ConnectOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.driver().register(|api| {
                let ops = Rc::new(ConnectOpsInner {
                    api,
                    connects: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsBatcher {
                    inner: ops,
                    feed: VecDeque::new(),
                })
            });

            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(
        &self,
        fd: RawFd,
        addr: SockAddr,
        sender: Sender<io::Result<()>>,
    ) -> io::Result<usize> {
        let result = syscall!(break libc::connect(fd, addr.as_ptr(), addr.len()));

        if let Poll::Ready(res) = result {
            res?;
        }

        let item = Item { fd, sender };
        let id = self.0.connects.borrow_mut().insert(item);

        self.0.api.register(fd, id, Interest::Writable);

        Ok(id)
    }
}

impl Handler for ConnectOpsBatcher {
    fn readable(&mut self, id: usize) {
        log::debug!("ConnectFD is readable {:?}", id);
        self.feed.push_back((id, Change::Readable));
    }

    fn writable(&mut self, id: usize) {
        log::debug!("ConnectFD is writable {:?}", id);
        self.feed.push_back((id, Change::Writable));
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.feed.push_back((id, Change::Error(err)));
    }

    fn commit(&mut self) {
        if self.feed.is_empty() {
            return;
        }
        log::debug!("Commit connect driver changes, num: {:?}", self.feed.len());

        let mut connects = self.inner.connects.borrow_mut();

        for (id, change) in self.feed.drain(..) {
            if connects.contains(id) {
                let item = connects.remove(id);
                match change {
                    Change::Readable => unreachable!(),
                    Change::Writable => {
                        let mut err: libc::c_int = 0;
                        let mut err_len =
                            std::mem::size_of::<libc::c_int>() as libc::socklen_t;

                        let res = syscall!(libc::getsockopt(
                            item.fd.as_raw_fd(),
                            libc::SOL_SOCKET,
                            libc::SO_ERROR,
                            &mut err as *mut _ as *mut _,
                            &mut err_len
                        ));

                        let res = if err == 0 {
                            res.map(|_| ())
                        } else {
                            Err(io::Error::from_raw_os_error(err))
                        };

                        self.inner.api.unregister_all(item.fd);
                        let _ = item.sender.send(res);
                    }
                    Change::Error(err) => {
                        let _ = item.sender.send(Err(err));
                        self.inner.api.unregister_all(item.fd);
                    }
                }
            }
        }
    }
}
