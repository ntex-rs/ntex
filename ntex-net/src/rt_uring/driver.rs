use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_neon::driver::io_uring::{cqueue, opcode, opcode2, squeue::Entry, types::Fd};
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
        const NO_ZC        = 0b1000_0000;
    }
}

const ZC_SIZE: u32 = 1536;
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
        result: Option<io::Result<usize>>,
    },
    Poll {
        id: usize,
        ctx: IoContext,
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
    feed: Cell<Option<Box<Vec<usize>>>>,
    delayd_drop: Cell<bool>,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
    default_flags: Flags,
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
                let default_flags = if api.is_supported(opcode::SendZc::CODE) {
                    Flags::empty()
                } else {
                    Flags::NO_ZC
                };
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
                    default_flags,
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

    pub(crate) fn register(&self, io: Socket, context: IoContext, zc: bool) -> StreamCtl {
        let ctx = context.clone();
        let item = StreamItem {
            io,
            rd_op: None,
            wr_op: None,
            ref_count: 1,
            flags: if zc {
                self.0.default_flags
            } else {
                Flags::NO_ZC
            },
            context: Some(context),
        };

        let id = self.0.with(move |st| {
            // handle RDHUP event
            let op = opcode::PollAdd::new(item.fd(), libc::POLLRDHUP as u32).build();
            let id = st.streams.insert(item);
            let op_id = st.ops.insert(Some(Operation::Poll { id, ctx })) as u32;
            self.0.api.submit(op_id, op);
            id
        });
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
                            st.recv(id, ctx, false, &self.inner.api);
                        }
                    }
                }
                Operation::Send {
                    id,
                    buf,
                    ctx,
                    result,
                } => {
                    ctx.release_write_buf(buf, Poll::Pending);
                    if let Some(item) = st.streams.get_mut(id) {
                        log::trace!("{}: Send canceled: {:?}", ctx.tag(), item.fd());
                        item.wr_op.take();
                        item.flags.remove(Flags::WR_CANCELING);
                        if item.flags.contains(Flags::WR_REISSUE) {
                            item.flags.remove(Flags::WR_REISSUE);
                            st.send(id, ctx, &self.inner.api);
                        }
                    }
                }
                Operation::Nop
                | Operation::Poll { .. }
                | Operation::Close { .. }
                | Operation::Shutdown { .. } => {}
            })
    }

    fn completed(&mut self, user_data: usize, flags: u32, res: io::Result<usize>) {
        self.inner.with(|st| {
            match st.ops[user_data].take().unwrap() {
                Operation::Recv { id, mut buf, ctx } => {
                    // reset op reference
                    st.streams.get_mut(id).map(|item| item.rd_op.take());

                    // handle WouldBlock
                    if matches!(res, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)) {
                        log::error!("{}: Received WouldBlock {:?}, id: {:?}", ctx.tag(), res, ctx.id());
                        st.recv_more(id, ctx, buf, &self.inner.api);
                    } else {
                        let mut total = 0;
                        let res = Poll::Ready(res.map(|size| {
                            unsafe { buf.advance_mut(size) };
                            total = size;
                        }));

                        // handle IORING_CQE_F_SOCK_NONEMPTY flag
                        if cqueue::sock_nonempty(flags) && matches!(res, Poll::Ready(Ok(()))) && total != 0 {
                            st.recv_more(id, ctx, buf, &self.inner.api);
                        } else if ctx.release_read_buf(total, buf, res) == IoTaskStatus::Io {
                            st.recv(id, ctx, self.inner.api.is_new(), &self.inner.api);
                        }
                    }
                }
                Operation::Send { id, buf, ctx, result } => {
                    if cqueue::notif(flags) {
                        if ctx.release_write_buf(buf, Poll::Ready(result.unwrap())) == IoTaskStatus::Io {
                            st.send(id, ctx, &self.inner.api);
                        }
                    } else if cqueue::more(flags) {
                        // reset op reference
                        st.streams.get_mut(id).map(|item| item.wr_op.take());
                        // try to send next chunk
                        if res.is_ok() {
                            st.send(id, ctx.clone(), &self.inner.api);
                        }
                        // insert op back for "notify" handling
                        st.ops[user_data] = Some(Operation::Send { id, buf, ctx, result: Some(res) });
                        return
                    } else {
                        // reset op reference
                        st.streams.get_mut(id).map(|item| item.wr_op.take());

                        // release buffer and try to send next chunk
                        if ctx.release_write_buf(buf, Poll::Ready(res)) == IoTaskStatus::Io {
                            st.send(id, ctx, &self.inner.api);
                        }
                    }
                }
                Operation::Poll { id, ctx } => {
                    if !ctx.is_stopped() {
                        if let Err(err) = res {
                            ctx.stop(Some(err));
                        } else {
                            ctx.init_shutdown();
                        }
                    }
                }
                Operation::Shutdown { tx } => {
                    if let Some(tx) = tx {
                        let _ = tx.send(res.map(|_| ()));
                    }
                }
                Operation::Close { id } => {
                    let item = st.streams.remove(id);
                    mem::forget(item.io);
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
                        let (op_id, entry) = st.close(id);
                        self.inner.api.submit(op_id, entry);
                    }
                }
                self.inner.feed.set(Some(feed));
            }
        })
    }
}

impl StreamOpsStorage {
    fn close(&mut self, id: usize) -> (u32, Entry) {
        let item = &self.streams[id];
        let entry = opcode::Close::new(item.fd()).build();
        log::trace!("{}: Close ({:?})", item.tag(), item.fd());

        (self.add_operation(Operation::Close { id }), entry)
    }

    fn recv(&mut self, id: usize, ctx: IoContext, poll_first: bool, api: &DriverApi) {
        if !self.streams.contains(id) {
            return;
        }

        let item = &mut self.streams[id];
        if item.rd_op.is_none() {
            let mut buf = ctx.get_read_buf();
            if buf.remaining_mut() < self.lw {
                buf.reserve(self.hw);
            }
            let s = buf.chunk_mut();
            let buf_ptr = s.as_mut_ptr();
            let buf_len = s.len() as u32;
            let op_id = self.ops.insert(Some(Operation::Recv { id, buf, ctx })) as u32;
            item.rd_op = NonZeroU32::new(op_id);

            api.submit_inline(op_id, move |entry| {
                let op = opcode2::Recv::with(entry, item.fd()).buffer(buf_ptr, buf_len);
                if poll_first {
                    op.ioprio(IORING_RECVSEND_POLL_FIRST);
                };
            });
        } else if item.flags.contains(Flags::RD_CANCELING) {
            item.flags.insert(Flags::RD_REISSUE);
        }
    }

    fn recv_more(&mut self, id: usize, ctx: IoContext, mut buf: BytesVec, api: &DriverApi) {
        if buf.remaining_mut() < self.lw {
            buf.reserve(self.hw);
        }
        let slice = buf.chunk_mut();
        let buf_ptr = slice.as_mut_ptr();
        let buf_len = slice.len() as u32;
        let item = &mut self.streams[id];
        let op_id = self.ops.insert(Some(Operation::Recv { id, buf, ctx })) as u32;
        item.rd_op = NonZeroU32::new(op_id);

        api.submit_inline(op_id, move |entry| {
            opcode2::Recv::with(entry, item.fd()).buffer(buf_ptr, buf_len);
        });
    }

    fn send(&mut self, id: usize, ctx: IoContext, api: &DriverApi) {
        let item = &mut self.streams[id];

        if item.wr_op.is_none() {
            if let Some(buf) = ctx.get_write_buf() {
                let slice = buf.chunk();
                let buf_ptr = slice.as_ptr();
                let buf_len = slice.len() as u32;
                let op_id = self.ops.insert(Some(Operation::Send {
                    id,
                    buf,
                    ctx,
                    result: None,
                })) as u32;
                item.wr_op = NonZeroU32::new(op_id);

                api.submit_inline(op_id, move |entry| {
                    if item.flags.contains(Flags::NO_ZC) || buf_len <= ZC_SIZE {
                        opcode2::Send::with(entry, item.fd()).buffer(buf_ptr, buf_len);
                    } else {
                        opcode2::SendZc::with(entry, item.fd()).buffer(buf_ptr, buf_len);
                    }
                });
            }
        } else if item.flags.contains(Flags::WR_CANCELING) {
            item.flags.insert(Flags::WR_REISSUE);
        }
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
            .map_err(|_| io::Error::other("gone"))
            .and_then(|item| item)
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|storage| f(&storage.streams[self.id].io))
    }

    pub(crate) fn resume_read(&self, ctx: &IoContext) {
        self.inner
            .with(|st| st.recv(self.id, ctx.clone(), false, &self.inner.api))
    }

    pub(crate) fn resume_write(&self, ctx: &IoContext) {
        self.inner
            .with(|storage| storage.send(self.id, ctx.clone(), &self.inner.api))
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
                let (op_id, entry) = storage.close(self.id);
                self.inner.api.submit(op_id, entry);
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
