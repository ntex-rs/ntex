use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{cell::Cell, io, rc::Rc};

use ntex_io::IoContext;
use ntex_rt::Arbiter;
use ntex_util::channel::pool;
use slab::Slab;
use socket2::Socket;

use super::{Driver, DriverApi, Handler, Overlapped, ops};

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

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    rd_op: ops::ReadOperation,
    wr_op: ops::WriteOperation,
}

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: DriverApi,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
}

struct StreamOpsStorage {
    streams: Slab<Box<StreamItem>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Driver) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
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
        let sock = io.as_raw_socket();
        if let Err(e) = self.0.api.attach(sock as _) {
            #[cfg(feature = "trace")]
            log::trace!("{}: Register failed({:?} {e:?})", ctx.tag(), io);
            ctx.update_write_status(Err(e));
        } else {
            #[cfg(feature = "trace")]
            log::trace!("{}: Registered({:?})", ctx.tag(), sock);
        }

        let mut storage = self.0.storage.take().unwrap();
        let entry = storage.streams.vacant_entry();
        let id = entry.key();

        // read op
        let rd_op = ops::ReadOperation::new(id, sock, ctx.clone(), &self.0.api);

        // write op
        let wr_op = ops::WriteOperation::new(id, sock, ctx, &self.0.api);

        entry.insert(Box::new(StreamItem { io, rd_op, wr_op }));
        self.0.storage.set(Some(storage));

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

impl Handler for StreamOpsHandler {
    fn completed(&mut self, udata: u32, res: io::Result<usize>, optr: *mut Overlapped) {
        match udata {
            ops::RD_OP => ops::ReadOperation::completed(res, optr),
            ops::WR_OP => ops::WriteOperation::completed(res, optr),
            _ => log::warn!("Unknown operation: {udata}"),
        }
    }

    fn cleanup(&mut self) {}
}

impl StreamOpsStorage {
    fn write(&self, _id: usize, _api: &DriverApi) {}

    fn pause(&self, _id: usize, _api: &DriverApi) {}
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
        self.rd_op.tag()
    }

    fn socket(&self) -> RawSocket {
        self.io.as_raw_socket()
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        println!("shutdown");
        Ok(())
    }

    pub(crate) fn read(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.rd_op.read();
            }
        });
    }

    pub(crate) fn write(&self) {
        self.inner.with(|st| st.write(self.id, &self.inner.api));
    }

    pub(crate) fn pause(&self) {
        self.inner.with(|st| st.pause(self.id, &self.inner.api));
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {}
}

impl WeakStreamCtl {
    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|st| f(&st.streams[self.id].io))
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {}
}
