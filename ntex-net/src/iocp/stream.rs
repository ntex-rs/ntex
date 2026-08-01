use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc};

use ntex_bytes::{BufMut, BytePage, BytePages, BytesMut};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::Arbiter;
use ntex_util::channel::pool;
use slab::Slab;
use socket2::Socket;

use super::driver::{Driver, DriverApi, Handler};
use crate::helpers::Queue;

#[derive(Clone)]
pub(crate) struct StreamOps(Rc<StreamOpsInner>);

pub(crate) struct StreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

pub(crate) struct WeakStreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

enum IdType {
    Stream(u32),
    Weak(u32),
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const RD_CANCELING = 0b0000_0001;
        const RD_REISSUE   = 0b0000_0010;
        const RD_MORE      = 0b0000_0100;
        const WR_CANCELING = 0b0000_1000;
        const WR_REISSUE   = 0b0001_0000;
        const NO_ZC        = 0b0010_0000;
        const DROPPED_PRI  = 0b0100_0000;
        const DROPPED_SEC  = 0b1000_0000;
    }
}

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    flags: Flags,
    rd_op: Overlapped,
    wr_op: Overlapped,
    ctx: IoContext,
}

#[derive(Debug)]
enum Operation {
    Recv {
        id: usize,
        buf: BytesMut,
    },
    Send {
        id: usize,
        buf: BytePage,
        result: Option<io::Result<usize>>,
    },
    Poll {
        id: usize,
    },
    Shutdown {
        tx: Option<pool::Sender<io::Result<()>>>,
    },
    Close {
        id: usize,
    },
    Nop,
}

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: DriverApi,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
    default_flags: Flags,
}

struct StreamOpsStorage {
    ops: Slab<Option<Operation>>,
    streams: Slab<StreamItem>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Driver) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                let mut ops = Slab::new();
                ops.insert(Some(Operation::Nop));

                let ops = Rc::new(StreamOpsInner {
                    api,
                    default_flags,
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        ops,
                        streams: Slab::new(),
                    }))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    pub(crate) fn register(self, io: Socket, ctx: IoContext) -> (StreamCtl, WeakStreamCtl) {
        let fd = io.as_raw_handle();
        let item = StreamItem {
            io,
            ctx,
            rd_op: None,
            wr_op: None,
            flags: self.0.default_flags,
        };
        self.0.api.attach(fd);

        (
            StreamCtl {
                id,
                inner: self.0.clone(),
            },
            WeakStreamCtl {
                id,
                inner: self.0.clone(),
            },
        )
    }
}

impl Operation {
    fn shutdown(tx: pool::Sender<io::Result<()>>) -> Self {
        Operation::Shutdown { tx: Some(tx) }
    }
}

impl Handler for StreamOpsHandler {
    fn completed(&mut self, user_data: u32, res: io::Result<usize>) {}

    fn cleanup(&mut self) {}
}

impl StreamOpsStorage {
    fn recv(&mut self, id: usize, api: &DriverApi) {}

    fn send(&mut self, id: usize, api: &DriverApi) {}

    fn add_operation(&mut self, op: Operation) -> u32 {}

    fn pause_read(&mut self, id: usize, api: &DriverApi) {}

    fn drop_stream(&mut self, id: usize, api: &DriverApi) {}

    fn drop_weak_stream(&mut self, id: usize) {}
}

impl StreamOpsInner {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StreamOpsStorage) -> R,
    {
        let mut storage = self.storage.take().unwrap();
        let result = f(&mut storage);
        self.storage.set(Some(storage));
        result
    }
}

impl StreamItem {
    fn tag(&self) -> &'static str {
        self.ctx.tag()
    }

    fn handle(&self) -> RawHandle {
        Fd(self.io.as_raw_handle())
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        todo!()
    }

    pub(crate) fn resume_read(&self) {
        todo!()
    }

    pub(crate) fn resume_write(&self) {
        todo!()
    }

    pub(crate) fn pause_read(&self) {
        todo!()
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        todo!()
    }
}

impl WeakStreamCtl {
    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        todo!()
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        todo!()
    }
}
