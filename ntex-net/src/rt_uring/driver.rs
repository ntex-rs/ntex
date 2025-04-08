use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc, task::Poll};

use io_uring::{opcode, squeue::Entry, types::Fd};
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::IoContext;
use ntex_neon::{driver::DriverApi, driver::Handler, Runtime};
use ntex_util::channel::oneshot;
use slab::Slab;
use socket2::Socket;

pub(crate) struct StreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
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

struct StreamItem {
    io: Socket,
    flags: Flags,
    rd_op: Option<NonZeroU32>,
    wr_op: Option<NonZeroU32>,
    ref_count: u8,
    context: Option<IoContext>,
}

enum Operation {
    Recv {
        id: usize,
        buf: BytesVec,
        ctx: IoContext,
    },
    Send {
        id: usize,
        buf: BytesVec,
        ctx: IoContext,
    },
    Shutdown {
        tx: Option<oneshot::Sender<io::Result<()>>>,
    },
    Nop,
}

pub(crate) struct StreamOps(Rc<StreamOpsInner>);

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: DriverApi,
    feed: Cell<Option<Box<Vec<usize>>>>,
    delayd_drop: Cell<bool>,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
}

struct StreamOpsStorage {
    ops: Slab<Operation>,
    streams: Slab<StreamItem>,
}

impl StreamOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.register_handler(|api| {
                if !api.is_supported(opcode::Recv::CODE) {
                    panic!("opcode::Recv is required for io-uring support");
                }
                if !api.is_supported(opcode::Send::CODE) {
                    panic!("opcode::Send is required for io-uring support");
                }
                if !api.is_supported(opcode::Close::CODE) {
                    panic!("opcode::Close is required for io-uring support");
                }
                if !api.is_supported(opcode::Shutdown::CODE) {
                    panic!("opcode::Shutdown is required for io-uring support");
                }

                let mut ops = Slab::new();
                ops.insert(Operation::Nop);

                let ops = Rc::new(StreamOpsInner {
                    api,
                    feed: Cell::new(Some(Box::new(Vec::new()))),
                    delayd_drop: Cell::new(false),
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

    pub(crate) fn register(&self, io: Socket, context: IoContext) -> StreamCtl {
        let item = StreamItem {
            io,
            rd_op: None,
            wr_op: None,
            ref_count: 1,
            context: Some(context),
            flags: Flags::empty(),
        };
        let id = self.0.with(|st| st.streams.insert(item));
        StreamCtl {
            id,
            inner: self.0.clone(),
        }
    }

    pub(crate) fn active_ops() -> usize {
        Self::current().0.with(|st| st.streams.len())
    }
}

impl Clone for StreamOps {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Handler for StreamOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        self.inner.with(|st| match st.ops.remove(user_data) {
            Operation::Recv { id, buf, ctx } => {
                ctx.release_read_buf(0, buf, Poll::Pending);
                if let Some(item) = st.streams.get_mut(id) {
                    log::trace!("{}: Recv canceled {:?}", ctx.tag(), item.fd());
                    item.rd_op.take();
                    item.flags.remove(Flags::RD_CANCELING);
                    if item.flags.contains(Flags::RD_REISSUE) {
                        item.flags.remove(Flags::RD_REISSUE);

                        let result = st.recv(id, ctx);
                        if let Some((id, op)) = result {
                            self.inner.api.submit(id, op);
                        }
                    }
                }
            }
            Operation::Send { id, buf, ctx } => {
                ctx.release_write_buf(buf, Poll::Pending);
                if let Some(item) = st.streams.get_mut(id) {
                    log::trace!("{}: Send canceled: {:?}", ctx.tag(), item.fd());
                    item.wr_op.take();
                    item.flags.remove(Flags::WR_CANCELING);
                    if item.flags.contains(Flags::WR_REISSUE) {
                        item.flags.remove(Flags::WR_REISSUE);

                        let result = st.send(id, ctx);
                        if let Some((id, op)) = result {
                            self.inner.api.submit(id, op);
                        }
                    }
                }
            }
            Operation::Nop | Operation::Shutdown { .. } => {}
        })
    }

    fn completed(&mut self, user_data: usize, flags: u32, res: io::Result<usize>) {
        self.inner.with(|st| {
            match st.ops.remove(user_data) {
                Operation::Recv { id, mut buf, ctx } => {
                    // reset op reference
                    if let Some(item) = st.streams.get_mut(id) {
                        item.rd_op.take();
                        log::trace!("{}: Recv({:?}) res: {res:?}", ctx.tag(), item.fd());
                    }

                    // handle WouldBlock
                    if matches!(res, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)) {
                        if let Some((id, op)) = st.recv(id, ctx) {
                            self.inner.api.submit(id, op);
                        }
                    } else {
                        let mut total = 0;
                        let res = Poll::Ready(res.map(|size| {
                            unsafe { buf.advance_mut(size) };
                            total = size;
                        }));

                        // set read buf
                        if ctx.release_read_buf(total, buf, res) && ctx.is_read_ready() {
                            if let Some((id, op)) = st.recv(id, ctx) {
                                self.inner.api.submit(id, op);
                            }
                        } else {
                            log::trace!("{}: Recv to pause", ctx.tag());
                        }
                    }
                }
                Operation::Send { id, buf, ctx } => {
                    // reset op reference
                    if let Some(item) = st.streams.get_mut(id) {
                        item.wr_op.take();
                        log::trace!("{}: Sent({:?}) res: {res:?}", ctx.tag(), item.fd());
                    }

                    // set read buf
                    if ctx.release_write_buf(buf, Poll::Ready(res)) {
                        if let Some((id, op)) = st.send(id, ctx) {
                            self.inner.api.submit(id, op);
                        }
                    }
                }
                Operation::Shutdown { tx } => {
                    if let Some(tx) = tx {
                        let _ = tx.send(res.map(|_| ()));
                    }
                }
                Operation::Nop => {}
            }

            // extra
            if self.inner.delayd_drop.get() {
                self.inner.delayd_drop.set(false);
                let mut feed = self.inner.feed.take().unwrap();
                for id in feed.drain(..) {
                    st.streams[id].ref_count -= 1;
                    if st.streams[id].ref_count == 0 {
                        let (id, entry) = st.close(id);
                        self.inner.api.submit(id, entry);
                    }
                }
                self.inner.feed.set(Some(feed));
            }
        })
    }
}

impl StreamOpsStorage {
    fn close(&mut self, id: usize) -> (u32, Entry) {
        let item = self.streams.remove(id);
        let fd = item.fd();
        let entry = opcode::Close::new(fd).build();
        log::trace!("{}: Close ({fd:?})", item.tag());

        mem::forget(item.io);
        (self.ops.insert(Operation::Nop) as u32, entry)
    }

    fn recv(&mut self, id: usize, ctx: IoContext) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.rd_op.is_none() {
            let fd = item.fd();
            let (mut buf, hw, lw) = ctx.get_read_buf();
            log::trace!("{}: Recv resume ({fd:?})", ctx.tag());
            let remaining = buf.remaining_mut();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            let slice = buf.chunk_mut();
            let op = opcode::Recv::new(fd, slice.as_mut_ptr(), slice.len() as u32).build();
            let op_id = self.ops.insert(Operation::Recv { id, buf, ctx }) as u32;
            item.rd_op = NonZeroU32::new(op_id);
            return Some((op_id, op));
        } else if item.flags.contains(Flags::RD_CANCELING) {
            item.flags.insert(Flags::RD_REISSUE);
        }
        None
    }

    fn send(&mut self, id: usize, ctx: IoContext) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.wr_op.is_none() {
            if let Some(buf) = ctx.get_write_buf() {
                let fd = item.fd();
                log::trace!("{}: Send resume ({fd:?}) len: {:?}", ctx.tag(), buf.len());

                let slice = buf.chunk();
                let op = opcode::Send::new(fd, slice.as_ptr(), slice.len() as u32).build();
                let op_id = self.ops.insert(Operation::Send { id, buf, ctx }) as u32;
                item.wr_op = NonZeroU32::new(op_id);
                return Some((op_id, op));
            }
        } else if item.flags.contains(Flags::WR_CANCELING) {
            item.flags.insert(Flags::WR_REISSUE);
        }
        None
    }
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
    fn fd(&self) -> Fd {
        Fd(self.io.as_raw_fd())
    }

    fn tag(&self) -> &'static str {
        self.context
            .as_ref()
            .map(|ctx| ctx.tag())
            .unwrap_or_default()
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        self.inner
            .with(|storage| {
                let (tx, rx) = oneshot::channel();
                let item = &mut storage.streams[self.id];
                let id = storage.ops.insert(Operation::Shutdown { tx: Some(tx) });

                self.inner.api.submit(
                    id as u32,
                    opcode::Shutdown::new(item.fd(), libc::SHUT_RDWR).build(),
                );
                rx
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "gone"))
            .and_then(|item| item)
            .map(|_| ())
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|storage| f(&storage.streams[self.id].io))
    }

    pub(crate) fn resume_read(&self, ctx: IoContext) {
        self.inner.with(|storage| {
            let result = storage.recv(self.id, ctx);
            if let Some((id, op)) = result {
                self.inner.api.submit(id, op);
            }
        })
    }

    pub(crate) fn resume_write(&self, ctx: IoContext) {
        self.inner.with(|storage| {
            let result = storage.send(self.id, ctx);
            if let Some((id, op)) = result {
                self.inner.api.submit(id, op);
            }
        })
    }

    pub(crate) fn pause_read(&self) {
        self.inner.with(|storage| {
            let item = &mut storage.streams[self.id];

            if let Some(rd_op) = item.rd_op {
                if !item.flags.contains(Flags::RD_CANCELING) {
                    item.flags.insert(Flags::RD_CANCELING);
                    self.inner.api.cancel(rd_op.get());
                    log::trace!("{}: Recv to pause ({:?})", item.tag(), item.fd());
                }
            }
        })
    }
}

impl Clone for StreamCtl {
    fn clone(&self) -> Self {
        self.inner.with(|storage| {
            storage.streams[self.id].ref_count += 1;
            Self {
                id: self.id,
                inner: self.inner.clone(),
            }
        })
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        if let Some(mut storage) = self.inner.storage.take() {
            storage.streams[self.id].ref_count -= 1;
            if storage.streams[self.id].ref_count == 0 {
                let (id, entry) = storage.close(self.id);
                self.inner.api.submit(id, entry);
            }
            self.inner.storage.set(Some(storage));
        } else {
            self.inner.delayd_drop.set(true);
            let mut feed = self.inner.feed.take().unwrap();
            feed.push(self.id);
            self.inner.feed.set(Some(feed));
        }
    }
}
