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

struct ConnectOpsInner {
    api: DriverApi,
    ops: RefCell<Slab<Sender<io::Result<()>>>>,
}

impl ConnectOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.driver().register(|api| {
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
        let id = self.0.ops.borrow_mut().insert(sender);
        self.0.api.submit(
            id as u32,
            opcode::Connect::new(Fd(fd), addr.as_ptr(), addr.len()).build(),
        );

        Ok(())
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
