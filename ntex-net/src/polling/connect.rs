use std::{cell::RefCell, io, os::fd::AsRawFd, os::fd::RawFd, rc::Rc, task::Poll};

use ntex_io::Io;
use ntex_rt::{Arbiter, syscall};
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{SockAddr, Socket};

use super::{Driver, DriverApi, Event, Handler, TcpStream, UnixStream, stream::StreamOps};
use crate::channel::{self, Receiver, Sender};

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

struct ConnectOpsBatcher {
    inner: Rc<ConnectOpsInner>,
}

struct Item {
    sock: Socket,
    cfg: SharedCfg,
    sender: Sender<Io>,
    uds: bool,
}

struct ConnectOpsInner {
    api: DriverApi,
    streams: StreamOps,
    connects: RefCell<Slab<Item>>,
}

impl Item {
    fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

impl ConnectOps {
    pub(crate) fn get(driver: &Driver) -> Self {
        let streams = StreamOps::get(driver);

        Arbiter::get_value(move || {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(ConnectOpsInner {
                    api,
                    streams,
                    connects: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsBatcher { inner: ops })
            });
            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(
        &self,
        sock: Socket,
        addr: &SockAddr,
        cfg: SharedCfg,
        uds: bool,
    ) -> Receiver<Io> {
        let result = syscall!(
            break libc::connect(sock.as_raw_fd(), addr.as_ptr().cast(), addr.len())
        );
        if let Poll::Ready(res) = result {
            match res {
                Err(err) => {
                    crate::helpers::close_socket(sock);
                    Receiver::new(Err(err))
                }
                Ok(_) => {
                    if uds {
                        Receiver::new(Ok(Io::new(
                            UnixStream(sock, self.0.streams.clone()),
                            cfg,
                        )))
                    } else {
                        Receiver::new(Ok(Io::new(
                            TcpStream(sock, self.0.streams.clone()),
                            cfg,
                        )))
                    }
                }
            }
        } else {
            // connect is async
            let (sender, rx) = channel::create();
            let fd = sock.as_raw_fd();
            let item = Item {
                sock,
                cfg,
                sender,
                uds,
            };
            let id = self.0.connects.borrow_mut().insert(item);
            self.0.api.attach(fd, id as u32, Event::writable(0));
            rx
        }
    }
}

impl Handler for ConnectOpsBatcher {
    fn event(&mut self, id: usize, ev: Event) {
        let mut connects = self.inner.connects.borrow_mut();
        if let Some(item) = connects.get(id) {
            log::trace!(
                "{}: {:?}-Connect rd({:?}) wr({:?}) {:?}",
                item.cfg.tag(),
                item.sock.as_raw_fd(),
                ev.readable,
                ev.writable,
                item.sock.local_addr().map(|s| s.as_socket())
            );

            if ev.writable {
                let item = connects.remove(id);
                let mut err: libc::c_int = 0;
                let mut err_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;

                let res = syscall!(libc::getsockopt(
                    item.fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&raw mut err).cast(),
                    &raw mut err_len
                ));

                let res = if err == 0 {
                    res
                } else {
                    Err(io::Error::from_raw_os_error(err))
                };

                self.inner.api.detach(item.fd(), id as u32);
                match res {
                    Ok(_) => {
                        if item.uds {
                            let _ = item.sender.send(Ok(Io::new(
                                UnixStream(item.sock, self.inner.streams.clone()),
                                item.cfg,
                            )));
                        } else {
                            let _ = item.sender.send(Ok(Io::new(
                                TcpStream(item.sock, self.inner.streams.clone()),
                                item.cfg,
                            )));
                        }
                    }
                    Err(err) => {
                        let _ = item.sender.send(Err(err));
                        crate::helpers::close_socket(item.sock);
                    }
                }
            } else {
                self.inner
                    .api
                    .attach(item.fd(), id as u32, Event::writable(0));
            }
        }
    }

    fn error(&mut self, id: usize, err: io::Error) {
        let mut connects = self.inner.connects.borrow_mut();

        if connects.contains(id) {
            let Item {
                sock, sender, cfg, ..
            } = connects.remove(id);
            log::trace!("{}: Connect {id:?} is failed {err:?}", cfg.tag());

            let _ = sender.send(Err(err));
            self.inner.api.detach(sock.as_raw_fd(), id as u32);
            crate::helpers::close_socket(sock);
        } else {
            log::error!("Connect {id:?} is failed {err:?}");
        }
    }

    fn tick(&mut self) {}

    fn cleanup(&mut self) {
        self.inner.connects.borrow_mut().clear();
    }
}
