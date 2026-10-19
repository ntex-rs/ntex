use std::{cell::RefCell, io, mem, os::fd::AsRawFd, os::fd::RawFd, rc::Rc, task::Poll};

use ntex_io::Io;
use ntex_rt::{Arbiter, syscall};
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{SockAddr, Socket};

use super::{Event, Handler, Reactor, ReactorApi, TcpStream, UnixStream, stream::StreamOps};
use crate::channel::{self, Cancel, Receiver, Sender};

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
    api: ReactorApi,
    streams: StreamOps,
    connects: RefCell<Slab<Item>>,
    /// Connects with dropped receivers, released by the next tick
    canceled: RefCell<Vec<u32>>,
}

impl Item {
    fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

impl ConnectOps {
    pub(crate) fn get(driver: &Reactor) -> Self {
        let streams = StreamOps::get(driver);

        Arbiter::get_value(move || {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(ConnectOpsInner {
                    api,
                    streams,
                    connects: RefCell::new(Slab::new()),
                    canceled: RefCell::new(Vec::new()),
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
        let result =
            syscall!(break libc::connect(sock.as_raw_fd(), addr.as_ptr().cast(), addr.len()));
        if let Poll::Ready(res) = result {
            match res {
                Err(err) => {
                    crate::helpers::close_socket(sock);
                    Receiver::new(Err(err))
                }
                Ok(_) => {
                    if uds {
                        Receiver::new(Ok(Io::new(UnixStream(sock, self.0.streams.clone()), cfg)))
                    } else {
                        Receiver::new(Ok(Io::new(TcpStream(sock, self.0.streams.clone()), cfg)))
                    }
                }
            }
        } else {
            // connect is async
            let fd = sock.as_raw_fd();
            let mut connects = self.0.connects.borrow_mut();
            let entry = connects.vacant_entry();
            let id = entry.key() as u32;
            let (sender, rx) = channel::with_cancel(self.0.clone(), id);
            entry.insert(Item {
                sock,
                cfg,
                sender,
                uds,
            });
            drop(connects);
            self.0.api.attach(fd, id, Event::writable(0));
            rx
        }
    }
}

impl Cancel for ConnectOpsInner {
    fn cancel(&self, id: u32) {
        // the receiver can be dropped while connects are borrowed
        self.canceled.borrow_mut().push(id);
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
                    Ok(_) if item.sender.is_canceled() => {
                        crate::helpers::close_socket(item.sock);
                    }
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

    fn tick(&mut self) {
        let canceled = mem::take(&mut *self.inner.canceled.borrow_mut());
        if canceled.is_empty() {
            return;
        }

        let mut connects = self.inner.connects.borrow_mut();
        for id in canceled {
            // the connect may have completed and its slot reused
            let id = id as usize;
            if connects
                .get(id)
                .is_some_and(|item| item.sender.is_canceled())
            {
                let item = connects.remove(id);
                log::trace!("{}: Connect {id:?} is canceled", item.cfg.tag());
                self.inner.api.detach(item.fd(), id as u32);
                crate::helpers::close_socket(item.sock);
            }
        }
    }

    fn cleanup(&mut self) {
        self.inner.canceled.borrow_mut().clear();
        self.inner.connects.borrow_mut().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the receiver of a pending connect releases the socket
    #[ntex::test]
    async fn dropped_receiver_cancels_connect() {
        let reactor = Reactor::new().unwrap();
        let ops = ConnectOps::get(&reactor);
        let mut handler = ConnectOpsBatcher {
            inner: ops.0.clone(),
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = SockAddr::from(listener.local_addr().unwrap());
        let connect = || {
            let sock = Socket::new(addr.domain(), socket2::Type::STREAM, None).unwrap();
            ops.connect(
                crate::helpers::prep_socket(sock).unwrap(),
                &addr,
                SharedCfg::default(),
                false,
            )
        };

        let rx = connect();
        if ops.0.connects.borrow().is_empty() {
            // connected without waiting, nothing to cancel
            return;
        }
        let rx2 = connect();
        assert_eq!(ops.0.connects.borrow().len(), 2);

        drop(rx);
        handler.tick();
        assert_eq!(ops.0.connects.borrow().len(), 1);

        // a completed connect does not cancel the reused slot
        let id = ops.0.connects.borrow().iter().next().unwrap().0;
        ops.0.canceled.borrow_mut().push(id as u32);
        handler.tick();
        assert_eq!(ops.0.connects.borrow().len(), 1);

        drop(rx2);
        handler.tick();
        assert!(ops.0.connects.borrow().is_empty());
        handler.cleanup();
    }
}
