use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_neon::driver::io_uring::{cqueue, opcode, squeue::Entry, types::Fd};
use ntex_neon::{driver::DriverApi, driver::Handler, Runtime};
use ntex_util::channel::pool;
use slab::Slab;
use socket2::Socket;

#[derive(Clone)]
pub(crate) struct StreamOps(Rc<StreamOpsInner>);

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

const IORING_RECVSEND_POLL_FIRST: u16 = 1;

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
        tx: Option<pool::Sender<io::Result<()>>>,
    },
    Nop,
}

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: DriverApi,
    feed: Cell<Option<Box<Vec<usize>>>>,
    delayd_drop: Cell<bool>,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
}

struct StreamOpsStorage {
    ops: Slab<Option<Operation>>,
    streams: Slab<StreamItem>,
    lw: usize,
    hw: usize,
}

impl StreamOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.register_handler(|api| {
                if !api.is_supported(opcode::SendZc::CODE) {
                    panic!("opcode::SendZc is required for io-uring support");
                }
                if !api.is_supported(opcode::Close::CODE) {
                    panic!("opcode::Close is required for io-uring support");
                }
                if !api.is_supported(opcode::Shutdown::CODE) {
                    panic!("opcode::Shutdown is required for io-uring support");
                }

                let mut ops = Slab::new();
                ops.insert(Some(Operation::Nop));

                let ops = Rc::new(StreamOpsInner {
                    api,
                    feed: Cell::new(Some(Box::new(Vec::new()))),
                    delayd_drop: Cell::new(false),
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        ops,
                        streams: Slab::new(),
                        lw: 1024,
                        hw: 1024 * 16,
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
            flags: Flags::empty(),
            context: Some(context),
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

impl Operation {
    fn shutdown(tx: pool::Sender<io::Result<()>>) -> Self {
        Operation::Shutdown { tx: Some(tx) }
    }
}

impl Handler for StreamOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        self.inner
            .with(|st| match st.ops.remove(user_data).unwrap() {
                Operation::Recv { id, buf, ctx } => {
                    ctx.release_read_buf(0, buf, Poll::Pending);
                    if let Some(item) = st.streams.get_mut(id) {
                        log::trace!("{}: Recv canceled {:?}", ctx.tag(), item.fd());
                        item.rd_op.take();
                        item.flags.remove(Flags::RD_CANCELING);
                        if item.flags.contains(Flags::RD_REISSUE) {
                            item.flags.remove(Flags::RD_REISSUE);
                            if let Some((id, op)) = st.recv(id, ctx, false) {
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
                            if let Some((id, op)) = st.send(id, ctx) {
                                self.inner.api.submit(id, op);
                            }
                        }
                    }
                }
                Operation::Nop | Operation::Shutdown { .. } => {}
            })
    }

    fn completed(&mut self, user_data: usize, flags: u32, mut res: io::Result<usize>) {
        self.inner.with(|st| {
            match st.ops[user_data].take().unwrap() {
                Operation::Recv { id, mut buf, ctx } => {
                    // reset op reference
                    st.streams.get_mut(id).map(|item| item.rd_op.take());

                    // handle WouldBlock
                    if matches!(res, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)) {
                        log::error!("{}: Received WouldBlock {:?}, id: {:?}", ctx.tag(), res, ctx.id());
                        let (id, op) = st.recv_more(id, ctx, buf);
                        self.inner.api.submit(id, op);
                    } else {
                        let mut total = 0;
                        let res = Poll::Ready(res.map(|size| {
                            unsafe { buf.advance_mut(size) };
                            total = size;
                        }));

                        // handle IORING_CQE_F_SOCK_NONEMPTY flag
                        if cqueue::sock_nonempty(flags) && matches!(res, Poll::Ready(Ok(()))) && total != 0 {
                            let (id, op) = st.recv_more(id, ctx, buf);
                            self.inner.api.submit(id, op);
                        } else if ctx.release_read_buf(total, buf, res) == IoTaskStatus::Io {
                            if let Some((id, op)) = st.recv(id, ctx, true) {
                                self.inner.api.submit(id, op);
                            }
                        }
                    }
                }
                Operation::Send { id, buf, ctx } => {
                    if cqueue::notif(flags) {
                        if res.is_ok() {
                            res = Ok(buf.len());
                        }
                        if ctx.release_write_buf(buf, Poll::Ready(res)) == IoTaskStatus::Io {
                            if let Some((id, op)) = st.send(id, ctx) {
                                self.inner.api.submit(id, op);
                            }
                        }
                    } else if cqueue::more(flags) {
                        // reset op reference
                        st.streams.get_mut(id).map(|item| item.wr_op.take());
                        // try to send next chunk
                        if let Some((id, op)) = st.send(id, ctx.clone()) {
                            self.inner.api.submit(id, op);
                        }
                        // insert op back for "notify" handling
                        st.ops[user_data] = Some(Operation::Send { id, buf, ctx });
                        return
                    } else {
                        // reset op reference
                        st.streams.get_mut(id).map(|item| item.wr_op.take());

                        // release buffer and try to send next chunk
                        if ctx.release_write_buf(buf, Poll::Ready(res)) == IoTaskStatus::Io {
                            if let Some((id, op)) = st.send(id, ctx) {
                                self.inner.api.submit(id, op);
                            }
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
            let _ = st.ops.remove(user_data);

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
        let entry = opcode::Close::new(item.fd()).build();
        log::trace!("{}: Close ({:?})", item.tag(), item.fd());

        mem::forget(item.io);
        (self.add_operation(Operation::Nop), entry)
    }

    fn recv(
        &mut self,
        id: usize,
        ctx: IoContext,
        poll_first: bool,
    ) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.rd_op.is_none() {
            let mut buf = ctx.get_read_buf();
            if buf.remaining_mut() < self.lw {
                buf.reserve(self.hw);
            }
            let s = buf.chunk_mut();
            let mut op = opcode::Recv::new(item.fd(), s.as_mut_ptr(), s.len() as u32);

            #[cfg(not(feature = "io-uring-compat"))]
            if poll_first {
                op = op.ioprio(IORING_RECVSEND_POLL_FIRST);
            };
            let op_id = self.ops.insert(Some(Operation::Recv { id, buf, ctx })) as u32;
            item.rd_op = NonZeroU32::new(op_id);
            return Some((op_id, op.build()));
        } else if item.flags.contains(Flags::RD_CANCELING) {
            item.flags.insert(Flags::RD_REISSUE);
        }
        None
    }

    fn recv_more(&mut self, id: usize, ctx: IoContext, mut buf: BytesVec) -> (u32, Entry) {
        if buf.remaining_mut() < self.lw {
            buf.reserve(self.hw);
        }
        let slice = buf.chunk_mut();
        let item = &mut self.streams[id];
        let op =
            opcode::Recv::new(item.fd(), slice.as_mut_ptr(), slice.len() as u32).build();
        let op_id = self.ops.insert(Some(Operation::Recv { id, buf, ctx })) as u32;
        item.rd_op = NonZeroU32::new(op_id);
        (op_id, op)
    }

    fn send(&mut self, id: usize, ctx: IoContext) -> Option<(u32, Entry)> {
        let item = &mut self.streams[id];

        if item.wr_op.is_none() {
            if let Some(buf) = ctx.get_write_buf() {
                let slice = buf.chunk();
                let op = opcode::SendZc::new(item.fd(), slice.as_ptr(), slice.len() as u32);
                let op_id = self.ops.insert(Some(Operation::Send { id, buf, ctx })) as u32;
                item.wr_op = NonZeroU32::new(op_id);
                return Some((op_id, op.build()));
            }
        } else if item.flags.contains(Flags::WR_CANCELING) {
            item.flags.insert(Flags::WR_REISSUE);
        }
        None
    }

    fn add_operation(&mut self, op: Operation) -> u32 {
        self.ops.insert(Some(op)) as u32
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
                let (tx, rx) = self.inner.pool.channel();
                let fd = storage.streams[self.id].fd();
                self.inner.api.submit(
                    storage.add_operation(Operation::shutdown(tx)),
                    opcode::Shutdown::new(fd, libc::SHUT_RDWR).build(),
                );
                rx
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "gone"))
            .and_then(|item| item)
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|storage| f(&storage.streams[self.id].io))
    }

    pub(crate) fn resume_read(&self, ctx: &IoContext) {
        self.inner.with(|st| {
            let result = st.recv(self.id, ctx.clone(), false);
            if let Some((id, op)) = result {
                self.inner.api.submit(id, op);
            }
        })
    }

    pub(crate) fn resume_write(&self, ctx: &IoContext) {
        self.inner.with(|storage| {
            let result = storage.send(self.id, ctx.clone());
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
