use std::{cell::RefCell, io, os::fd::RawFd, rc::Rc};

use io_uring::{opcode, types::Fd};
use ntex_neon::{driver::DriverApi, driver::Handler, Runtime};
use ntex_util::channel::oneshot::Sender;
use slab::Slab;
use socket2::SockAddr;

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

type Operations = RefCell<Slab<(Box<SockAddr>, Sender<io::Result<()>>)>>;

struct ConnectOpsInner {
    api: DriverApi,
    ops: Operations,
}

impl ConnectOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.register_handler(|api| {
                if !api.is_supported(opcode::Connect::CODE) {
                    panic!("opcode::Connect is required for io-uring support");
                }

                let ops = Rc::new(ConnectOpsInner {
                    api,
                    ops: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsHandler { inner: ops })
            });
            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(
        &self,
        fd: RawFd,
        addr: SockAddr,
        sender: Sender<io::Result<()>>,
    ) -> io::Result<()> {
        let addr2 = addr.clone();
        let mut ops = self.0.ops.borrow_mut();

        // addr must be stable, neon submits ops at the end of rt turn
        let addr = Box::new(addr);
        let (addr_ptr, addr_len) = (addr.as_ref().as_ptr(), addr.len());

        let id = ops.insert((addr, sender));
        self.0.api.submit(
            id as u32,
            opcode::Connect::new(Fd(fd), addr_ptr, addr_len).build(),
        );

        Ok(())
    }
}

impl Handler for ConnectOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        log::trace!("connect-op is canceled {:?}", user_data);

        self.inner.ops.borrow_mut().remove(user_data);
    }

    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>) {
        let (addr, tx) = self.inner.ops.borrow_mut().remove(user_data);
        log::trace!(
            "connect-op is completed {:?} result: {:?}, addr: {:?}",
            user_data,
            result,
            addr.as_socket()
        );

        let _ = tx.send(result.map(|_| ()));
    }
}
