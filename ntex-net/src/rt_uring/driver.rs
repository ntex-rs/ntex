use std::{cell::RefCell, io, mem, num::NonZeroU32, os, rc::Rc, task::Poll};

use io_uring::{opcode, squeue::Entry, types::Fd};
use ntex_neon::{driver::DriverApi, driver::Handler, Runtime};
use ntex_util::channel::oneshot;
use slab::Slab;

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::IoContext;

pub(crate) struct StreamCtl<T> {
    id: usize,
    inner: Rc<StreamOpsInner<T>>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const RD_CANCELING = 0b0000_0001;
        const RD_REISSUE   = 0b0000_0010;
        const WR_CANCELING = 0b0001_0000;
        const WR_REISSUE   = 0b0010_0000;
    }
}

struct StreamItem<T> {
    io: Option<T>,
    fd: Fd,
    context: IoContext,
    ref_count: usize,
    flags: Flags,
    rd_op: Option<NonZeroU32>,
    wr_op: Option<NonZeroU32>,
}

impl<T> StreamItem<T> {
    fn tag(&self) -> &'static str {
        self.context.tag()
    }
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
        tx: Option<oneshot::Sender<io::Result<i32>>>,
    },
    Nop,
}

pub(crate) struct StreamOps<T>(Rc<StreamOpsInner<T>>);

struct StreamOpsHandler<T> {
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamOpsInner<T> {
    api: DriverApi,
    feed: RefCell<Vec<usize>>,
    storage: RefCell<StreamOpsStorage<T>>,
}

struct StreamOpsStorage<T> {
    ops: Slab<Operation>,
    streams: Slab<StreamItem<T>>,
}

impl<T: os::fd::AsRawFd + 'static> StreamOps<T> {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.driver().register(|api| {
                if !api.is_supported(opcode::Recv::CODE) {
                    panic!("opcode::Recv is required for io-uring support");
                }
                if !api.is_supported(opcode::Send::CODE) {
                    panic!("opcode::Send is required for io-uring support");
                }
                if !api.is_supported(opcode::Close::CODE) {
                    panic!("opcode::Close is required for io-uring support");
                }

                let mut ops = Slab::new();
                ops.insert(Operation::Nop);

                let ops = Rc::new(StreamOpsInner {
                    api,
                    feed: RefCell::new(Vec::new()),
                    storage: RefCell::new(StreamOpsStorage {
                        ops,
                        streams: Slab::new(),
                    }),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
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
            flags: Flags::empty(),
        };
        let id = self.0.storage.borrow_mut().streams.insert(item);
        StreamCtl {
            id,
            inner: self.0.clone(),
        }
    }

    pub(crate) fn active_ops() -> usize {
        Self::current().with(|st| st.streams.len())
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StreamOpsStorage<T>) -> R,
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
    fn canceled(&mut self, user_data: usize) {
        let mut storage = self.inner.storage.borrow_mut();

        match storage.ops.remove(user_data) {
            Operation::Recv { id, buf, context } => {
                log::trace!("{}: Recv canceled {:?}", context.tag(), id);
                context.release_read_buf(buf);
                if let Some(item) = storage.streams.get_mut(id) {
                    item.rd_op.take();
                    item.flags.remove(Flags::RD_CANCELING);
                    if item.flags.contains(Flags::RD_REISSUE) {
                        item.flags.remove(Flags::RD_REISSUE);

                        let result = storage.recv(id, Some(context));
                        if let Some((id, op)) = result {
                            self.inner.api.submit(id, op);
                        }
                    }
                }
            }
            Operation::Send { id, buf, context } => {
                log::trace!("{}: Send canceled: {:?}", context.tag(), id);
                context.release_write_buf(buf);
                if let Some(item) = storage.streams.get_mut(id) {
                    item.wr_op.take();
                    item.flags.remove(Flags::WR_CANCELING);
                    if item.flags.contains(Flags::WR_REISSUE) {
                        item.flags.remove(Flags::WR_REISSUE);

                        let result = storage.send(id, Some(context));
                        if let Some((id, op)) = result {
                            self.inner.api.submit(id, op);
                        }
                    }
                }
            }
            Operation::Nop | Operation::Close { .. } => {}
        }
    }

    fn completed(&mut self, user_data: usize, flags: u32, result: io::Result<i32>) {
        let mut storage = self.inner.storage.borrow_mut();

        let op = storage.ops.remove(user_data);
        match op {
            Operation::Recv {
                id,
                mut buf,
                context,
            } => {
                let result = result.map(|size| {
                    unsafe { buf.advance_mut(size as usize) };
                    size as usize
                });

                // reset op reference
                if let Some(item) = storage.streams.get_mut(id) {
                    log::trace!(
                        "{}: Recv completed {:?}, res: {:?}, buf({})",
                        context.tag(),
                        item.fd,
                        result,
                        buf.remaining_mut()
                    );
                    item.rd_op.take();
                }

                // set read buf
                let tag = context.tag();
                if context.set_read_buf(result, buf).is_pending() {
                    if let Some((id, op)) = storage.recv(id, Some(context)) {
                        self.inner.api.submit(id, op);
                    }
                } else {
                    log::trace!("{}: Recv to pause", tag);
                }
            }
            Operation::Send { id, buf, context } => {
                // reset op reference
                let fd = if let Some(item) = storage.streams.get_mut(id) {
                    log::trace!(
                        "{}: Send completed: {:?}, res: {:?}, buf({})",
                        context.tag(),
                        item.fd,
                        result,
                        buf.len()
                    );
                    item.wr_op.take();
                    Some(item.fd)
                } else {
                    None
                };

                // set read buf
                let result = context.set_write_buf(result.map(|size| size as usize), buf);
                if result.is_pending() {
                    log::trace!("{}: Need to send more: {:?}", context.tag(), fd);
                    if let Some((id, op)) = storage.send(id, Some(context)) {
                        self.inner.api.submit(id, op);
                    }
                }
            }
            Operation::Close { tx } => {
                if let Some(tx) = tx {
                    let _ = tx.send(result);
                }
            }
            Operation::Nop => {}
        }

        // extra
        for id in self.inner.feed.borrow_mut().drain(..) {
            storage.streams[id].ref_count -= 1;
            if storage.streams[id].ref_count == 0 {
                let mut item = storage.streams.remove(id);

                log::trace!("{}: Drop io ({}), {:?}", item.tag(), id, item.fd);

                if let Some(io) = item.io.take() {
                    mem::forget(io);

                    let id = storage.ops.insert(Operation::Close { tx: None });
                    assert!(id < u32::MAX as usize);
                    self.inner
                        .api
                        .submit(id as u32, opcode::Close::new(item.fd).build());
                }
            }
        }
    }
}

impl<T> StreamOpsStorage<T> {
    fn recv(&mut self, id: usize, context: Option<IoContext>) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.rd_op.is_none() {
            if let Poll::Ready(mut buf) = item.context.get_read_buf() {
                log::trace!(
                    "{}: Recv resume ({}), {:?} rem: {:?}",
                    item.tag(),
                    id,
                    item.fd,
                    buf.remaining_mut()
                );

                let slice = buf.chunk_mut();
                let op = opcode::Recv::new(item.fd, slice.as_mut_ptr(), slice.len() as u32)
                    .build();

                let op_id = self.ops.insert(Operation::Recv {
                    id,
                    buf,
                    context: context.unwrap_or_else(|| item.context.clone()),
                });
                assert!(op_id < u32::MAX as usize);

                item.rd_op = NonZeroU32::new(op_id as u32);
                return Some((op_id as u32, op));
            }
        } else if item.flags.contains(Flags::RD_CANCELING) {
            item.flags.insert(Flags::RD_REISSUE);
        }
        None
    }

    fn send(&mut self, id: usize, context: Option<IoContext>) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.wr_op.is_none() {
            if let Poll::Ready(buf) = item.context.get_write_buf() {
                log::trace!(
                    "{}: Send resume ({}), {:?} len: {:?}",
                    item.tag(),
                    id,
                    item.fd,
                    buf.len()
                );

                let slice = buf.chunk();
                let op =
                    opcode::Send::new(item.fd, slice.as_ptr(), slice.len() as u32).build();

                let op_id = self.ops.insert(Operation::Send {
                    id,
                    buf,
                    context: context.unwrap_or_else(|| item.context.clone()),
                });
                assert!(op_id < u32::MAX as usize);

                item.wr_op = NonZeroU32::new(op_id as u32);
                return Some((op_id as u32, op));
            }
        } else if item.flags.contains(Flags::WR_CANCELING) {
            item.flags.insert(Flags::WR_REISSUE);
        }
        None
    }
}

impl<T> StreamCtl<T> {
    pub(crate) async fn close(self) -> io::Result<()> {
        let result = {
            let mut storage = self.inner.storage.borrow_mut();

            let (io, fd) = {
                let item = &mut storage.streams[self.id];
                (item.io.take(), item.fd)
            };
            if let Some(io) = io {
                mem::forget(io);

                let (tx, rx) = oneshot::channel();
                let id = storage.ops.insert(Operation::Close { tx: Some(tx) });
                assert!(id < u32::MAX as usize);

                drop(storage);
                self.inner
                    .api
                    .submit(id as u32, opcode::Close::new(fd).build());
                Some(rx)
            } else {
                None
            }
        };

        if let Some(rx) = result {
            rx.await
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "gone"))
                .and_then(|item| item)
                .map(|_| ())
        } else {
            Ok(())
        }
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&T>) -> R,
    {
        f(self.inner.storage.borrow().streams[self.id].io.as_ref())
    }

    pub(crate) fn resume_read(&self) {
        let result = self.inner.storage.borrow_mut().recv(self.id, None);
        if let Some((id, op)) = result {
            self.inner.api.submit(id, op);
        }
    }

    pub(crate) fn resume_write(&self) {
        let result = self.inner.storage.borrow_mut().send(self.id, None);
        if let Some((id, op)) = result {
            self.inner.api.submit(id, op);
        }
    }

    pub(crate) fn pause_read(&self) {
        let mut storage = self.inner.storage.borrow_mut();
        let item = &mut storage.streams[self.id];

        if let Some(rd_op) = item.rd_op {
            if !item.flags.contains(Flags::RD_CANCELING) {
                log::trace!("{}: Recv to pause ({}), {:?}", item.tag(), self.id, item.fd);
                item.flags.insert(Flags::RD_CANCELING);
                self.inner.api.cancel(rd_op.get());
            }
        }
    }
}

impl<T> Clone for StreamCtl<T> {
    fn clone(&self) -> Self {
        self.inner.storage.borrow_mut().streams[self.id].ref_count += 1;
        Self {
            id: self.id,
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for StreamCtl<T> {
    fn drop(&mut self) {
        if let Ok(mut storage) = self.inner.storage.try_borrow_mut() {
            storage.streams[self.id].ref_count -= 1;
            if storage.streams[self.id].ref_count == 0 {
                let mut item = storage.streams.remove(self.id);
                if let Some(io) = item.io.take() {
                    log::trace!("{}: Close io ({}), {:?}", item.tag(), self.id, item.fd);
                    mem::forget(io);

                    let id = storage.ops.insert(Operation::Close { tx: None });
                    assert!(id < u32::MAX as usize);
                    self.inner
                        .api
                        .submit(id as u32, opcode::Close::new(item.fd).build());
                }
            }
        } else {
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}
