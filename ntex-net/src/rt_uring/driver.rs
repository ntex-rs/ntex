use std::{cell::RefCell, collections::VecDeque, fmt, io, num::NonZeroU32, rc::Rc};

use io_uring::{opcode, types::Fd};
use ntex_neon::driver::op::Handler;
use ntex_neon::driver::{AsRawFd, DriverApi};
use ntex_neon::Runtime;
use ntex_util::channel::oneshot;
use slab::Slab;

use ntex_bytes::{BufMut, BytesVec};
use ntex_io::IoContext;

pub(crate) struct StreamCtl<T> {
    id: usize,
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamItem<T> {
    io: Option<T>,
    fd: Fd,
    context: IoContext,
    ref_count: usize,
    rd_op: Option<NonZeroU32>,
    wr_op: Option<NonZeroU32>,
}

enum Operation {
    Recv {
        id: usize,
        buf: BytesVec,
        context: IoContext,
    },
    Send {
        id: usize,
        buf: BytesVec,
        context: IoContext,
    },
    Close {
        tx: Option<oneshot::Sender<io::Result<()>>>,
    },
    Cancel {
        id: u32,
    },
    Nop,
}

pub(crate) struct StreamOps<T>(Rc<StreamOpsInner<T>>);

struct StreamOpsHandler<T> {
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamOpsInner<T> {
    api: DriverApi,
    feed: RefCell<VecDeque<usize>>,
    storage: RefCell<(Slab<StreamItem<T>>, Slab<Operation>)>,
}

impl<T: AsRawFd + 'static> StreamOps<T> {
    pub(crate) fn current() -> Self {
        Runtime::with_current(|rt| {
            if let Some(s) = rt.get::<Self>() {
                s
            } else {
                let mut inner = None;
                rt.driver().register_handler(|api| {
                    let mut storage = Slab::new();
                    storage.insert(Operation::Nop);

                    let ops = Rc::new(StreamOpsInner {
                        api,
                        feed: RefCell::new(VecDeque::new()),
                        storage: RefCell::new((Slab::new(), Slab::new())),
                    });
                    inner = Some(ops.clone());
                    Box::new(StreamOpsHandler { inner: ops })
                });

                let s = StreamOps(inner.unwrap());
                rt.insert(s.clone());
                s
            }
        })
    }

    pub(crate) fn register(&self, io: T, context: IoContext) -> StreamCtl<T> {
        let item = StreamItem {
            context,
            fd: Fd(io.as_raw_fd()),
            io: Some(io),
            ref_count: 1,
            rd_op: None,
            wr_op: None,
        };
        self.with(|streams| {
            let id = streams.0.insert(item);
            StreamCtl {
                id,
                inner: self.0.clone(),
            }
        })
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut (Slab<StreamItem<T>>, Slab<Operation>)) -> R,
    {
        f(&mut *self.0.storage.borrow_mut())
    }
}

impl<T> Clone for StreamOps<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Handler for StreamOpsHandler<T> {
    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>) {
        log::debug!("Op is completed {:?} result: {:?}", user_data, result);

        // let mut storage = self.inner.storage.borrow_mut();
        // for (id, flags, result) in self.feed.drain(..) {}

        // // extra
        // for id in self.inner.feed.borrow_mut().drain(..) {
        //     log::debug!("Drop io ({}), {:?}", id, storage.0[id].fd);

        //     storage.0[id].ref_count -= 1;
        //     if storage.0[id].ref_count == 0 {
        //         let item = storage.0.remove(id);
        //         if item.io.is_some() {
        //             // self.inner.api.unregister_all(item.fd);
        //         }
        //     }
        // }
    }
}

impl<T> StreamCtl<T> {
    pub(crate) async fn close(self) -> io::Result<()> {
        let result = self.with(|streams| {
            let item = &mut streams.0[self.id];
            if let Some(io) = item.io.take() {
                let (tx, rx) = oneshot::channel();
                let id = streams.1.insert(Operation::Close { tx: Some(tx) });
                assert!(id < u32::MAX as usize);
                self.inner
                    .api
                    .submit(id as u32, opcode::Close::new(item.fd).build());
                Some(rx)
            } else {
                None
            }
        });

        if let Some(rx) = result {
            rx.await
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "gone"))
                .and_then(|item| item)
        } else {
            Ok(())
        }
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&T>) -> R,
    {
        self.with(|streams| f(streams.0[self.id].io.as_ref()))
    }

    pub(crate) fn resume_read(&self) {
        self.with(|streams| {
            let item = &mut streams.0[self.id];

            if item.rd_op.is_none() {
                log::debug!("Resume io read ({}), {:?}", self.id, item.fd);
                let mut buf = item.context.get_read_buf();
                let slice = buf.chunk_mut();
                let op = opcode::Recv::new(item.fd, slice.as_mut_ptr(), slice.len() as u32)
                    .build();

                let id = streams.1.insert(Operation::Recv {
                    buf,
                    id: self.id,
                    context: item.context.clone(),
                });
                assert!(id < u32::MAX as usize);

                self.inner.api.submit(id as u32, op);
            }
        })
    }

    pub(crate) fn resume_write(&self) {
        self.with(|streams| {
            let item = &mut streams.0[self.id];

            if item.wr_op.is_none() {
                log::debug!("Resume io write ({}), {:?}", self.id, item.fd);
                //self.inner.api.unregister(item.fd, Interest::Readable);
            }
        })
    }

    pub(crate) fn pause_read(&self) {
        self.with(|streams| {
            let item = &mut streams.0[self.id];

            if let Some(rd_op) = item.rd_op {
                log::debug!("Pause io read ({}), {:?}", self.id, item.fd);
                let id = streams.1.insert(Operation::Cancel { id: rd_op.get() });
                assert!(id < u32::MAX as usize);
                self.inner.api.cancel(id as u32, rd_op.get());
            }
        })
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut (Slab<StreamItem<T>>, Slab<Operation>)) -> R,
    {
        let mut storage = self.inner.storage.borrow_mut();
        f(&mut *storage)
    }
}

impl<T> Clone for StreamCtl<T> {
    fn clone(&self) -> Self {
        self.with(|streams| {
            streams.0[self.id].ref_count += 1;
            Self {
                id: self.id,
                inner: self.inner.clone(),
            }
        })
    }
}

impl<T> Drop for StreamCtl<T> {
    fn drop(&mut self) {
        if let Ok(storage) = &mut self.inner.storage.try_borrow_mut() {
            log::debug!("Drop io ({}), {:?}", self.id, storage.0[self.id].fd);

            storage.0[self.id].ref_count -= 1;
            if storage.0[self.id].ref_count == 0 {
                let item = storage.0.remove(self.id);
                if item.io.is_some() {
                    let id = storage.1.insert(Operation::Close { tx: None });
                    assert!(id < u32::MAX as usize);
                    self.inner
                        .api
                        .submit(id as u32, opcode::Close::new(item.fd).build());
                }
            }
        } else {
            self.inner.feed.borrow_mut().push_back(self.id);
        }
    }
}

impl<T> PartialEq for StreamCtl<T> {
    #[inline]
    fn eq(&self, other: &StreamCtl<T>) -> bool {
        self.id == other.id && std::ptr::eq(&self.inner, &other.inner)
    }
}

impl<T: fmt::Debug> fmt::Debug for StreamCtl<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with(|streams| {
            f.debug_struct("StreamCtl")
                .field("id", &self.id)
                .field("io", &streams.0[self.id].io)
                .finish()
        })
    }
}
