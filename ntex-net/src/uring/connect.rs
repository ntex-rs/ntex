use std::{cell::RefCell, io, os::fd::AsRawFd, rc::Rc};

use ntex_io::Io;
use ntex_io_uring::{opcode, types::Fd};
use ntex_rt::Arbiter;
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{Domain, SockAddr, Socket};

use super::{Handler, Reactor, ReactorApi, TcpStream, UnixStream, stream::StreamOps};
use crate::channel::{self, Cancel, Receiver, Sender};

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

struct ConnectOpsHandler {
    inner: Rc<ConnectOpsInner>,
}

type Operations = RefCell<Slab<(Box<SockAddr>, Socket, Sender<Io>, SharedCfg)>>;

struct ConnectOpsInner {
    api: ReactorApi,
    streams: StreamOps,
    ops: Operations,
}

impl ConnectOps {
    pub(crate) fn get(reactor: &Reactor) -> Self {
        let streams = StreamOps::get(reactor);

        Arbiter::get_value(move || {
            let mut inner = None;
            reactor.register(|api| {
                assert!(
                    api.is_supported(opcode::Connect::CODE),
                    "opcode::Connect is required for io-uring support"
                );

                let ops = Rc::new(ConnectOpsInner {
                    api,
                    streams,
                    ops: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsHandler { inner: ops })
            });
            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(&self, sock: Socket, addr: SockAddr, cfg: SharedCfg) -> Receiver<Io> {
        let mut ops = self.0.ops.borrow_mut();

        // addr must be stable, neon submits ops at the end of rt turn
        let addr = Box::new(addr);
        let (addr_ptr, addr_len) = (addr.as_ref().as_ptr().cast(), addr.len());

        let fd = sock.as_raw_fd();
        let entry = ops.vacant_entry();
        let id = entry.key();
        let (sender, rx) = channel::with_cancel(self.0.clone(), id as u32);
        entry.insert((addr, sock, sender, cfg));
        self.0.api.submit(id as u32, move |entry| {
            opcode::Connect::new(entry, Fd(fd), addr_ptr, addr_len);
        });
        rx
    }
}

impl Cancel for ConnectOpsInner {
    fn cancel(&self, id: u32) {
        // the connect completes with a cancellation error, or is
        // completed already and the socket is closed then
        self.api.cancel(id);
    }
}

impl Handler for ConnectOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        log::trace!("connect-op is canceled {user_data:?}");
        self.inner.ops.borrow_mut().remove(user_data);
    }

    fn completed(&mut self, user_data: usize, _: u32, result: io::Result<usize>) {
        let (addr, sock, tx, cfg) = self.inner.ops.borrow_mut().remove(user_data);
        log::trace!(
            "{}: connect-op is completed {user_data:?} result: {result:?}, addr: {:?}",
            cfg.tag(),
            addr.as_socket()
        );

        match result {
            Ok(_) if tx.is_canceled() => crate::helpers::close_socket(sock),
            Ok(_) => {
                if sock.domain().is_ok_and(|d| d == Domain::UNIX) {
                    let _ = tx.send(Ok(Io::new(
                        UnixStream(sock, self.inner.streams.clone()),
                        cfg,
                    )));
                } else {
                    let _ = tx.send(Ok(Io::new(
                        TcpStream(sock, self.inner.streams.clone()),
                        cfg,
                    )));
                }
            }
            Err(err) => {
                let _ = tx.send(Err(err));
                crate::helpers::close_socket(sock);
            }
        }
    }

    fn tick(&mut self) {}

    fn cleanup(&mut self) {
        self.inner.ops.borrow_mut().clear();
    }
}
