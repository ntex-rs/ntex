use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc, task::Poll};

use io_uring::{cqueue, opcode, opcode2, types::Fd};
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::IoContext;
use ntex_rt::Arbiter;
use ntex_util::channel::pool;
use slab::Slab;
use socket2::Socket;

use super::driver::{Driver, DriverApi, Handler};

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

const ZC_SIZE: u32 = 1536;
const IORING_RECVSEND_POLL_FIRST: u16 = 1;

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    flags: Flags,
    rd_op: Option<NonZeroU32>,
    wr_op: Option<NonZeroU32>,
    ctx: IoContext,
}

#[derive(Debug)]
enum Operation {
    Recv {
        id: usize,
        buf: BytesVec,
    },
    Send {
        id: usize,
        buf: BytesVec,
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
    delayed: Cell<bool>,
    delayed_feed: Cell<Option<Box<Vec<IdType>>>>,
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
                    delayed: Cell::new(false),
                    delayed_feed: Cell::new(Some(Box::new(Vec::new()))),
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

    pub(crate) fn register(
        self,
        io: Socket,
        ctx: IoContext,
        zc: bool,
    ) -> (StreamCtl, WeakStreamCtl) {
        let item = StreamItem {
            io,
            ctx,
            rd_op: None,
            wr_op: None,
            flags: if zc { self.0.default_flags } else { Flags::NO_ZC },
        };

        let id = self.0.with(|st| {
            // handle RDHUP event
            let op = opcode::PollAdd::new(item.fd(), libc::POLLRDHUP as u32).build();
            let id = st.streams.insert(item);
            let op_id = st.ops.insert(Some(Operation::Poll { id })) as u32;
            self.0.api.submit(op_id, op);
            id
        });
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
    fn canceled(&mut self, user_data: usize) {
        self.inner
            .with(|st| match st.ops.remove(user_data).unwrap() {
                Operation::Recv { id, buf } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        log::trace!("{}: Recv canceled {:?}", item.tag(), item.fd());
                        item.rd_op.take();
                        item.flags.remove(Flags::RD_CANCELING);
                        item.ctx.release_read_buf(0, buf, Poll::Pending);
                        if item.flags.contains(Flags::RD_REISSUE) {
                            item.flags.remove(Flags::RD_REISSUE);
                            st.recv(id, false, &self.inner.api);
                        }
                    }
                }
                Operation::Send { id, buf, .. } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        log::trace!("{}: Send canceled: {:?}", item.tag(), item.fd());
                        item.wr_op.take();
                        item.flags.remove(Flags::WR_CANCELING);
                        item.ctx.release_write_buf(buf, Poll::Pending);
                        if item.flags.contains(Flags::WR_REISSUE) {
                            item.flags.remove(Flags::WR_REISSUE);
                            st.send(id, &self.inner.api);
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
                Operation::Recv { id, mut buf, } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        // reset op reference
                        let _ = item.rd_op.take();

                        // handle WouldBlock
                        if matches!(res, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)) {
                            log::error!("{}: Received WouldBlock {:?}, id: {:?}", item.tag(), res, item.ctx.id());
                            st.recv_more(id, buf, &self.inner.api);
                        } else {
                            let mut total = 0;
                            let res = Poll::Ready(res.map(|size| {
                                // SAFETY: kernel tells us how many bytes it read
                                unsafe { buf.advance_mut(size) };
                                total = size;
                            }).map_err(Some));

                            // handle IORING_CQE_F_SOCK_NONEMPTY flag
                            if cqueue::sock_nonempty(flags) && matches!(res, Poll::Ready(Ok(()))) && total != 0 {
                                // In case of disconnect, sock_nonempty is set to true.
                                // First completion contains data, second Recv(0)
                                // Before receiving Recv(0), POLLRDHUP is triggered
                                // Driver must read all recv() call before handling
                                // disconnects
                                item.flags.insert(Flags::RD_MORE);
                                st.recv_more(id, buf, &self.inner.api);
                            } else {
                                item.flags.remove(Flags::RD_MORE);
                                if item.ctx.release_read_buf(total, buf, res).ready() {
                                    st.recv(id, self.inner.api.is_new(), &self.inner.api);
                                }
                            }
                        }
                    }
                }
                Operation::Send { id, buf, result } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        if cqueue::notif(flags) {
                            if item.ctx.release_write_buf(buf, Poll::Ready(result.unwrap())).ready() {
                                st.send(id, &self.inner.api);
                            }
                        } else if cqueue::more(flags) {
                            // reset op reference
                            item.wr_op.take();

                            // try to send next chunk
                            if res.is_ok() {
                                st.send(id, &self.inner.api);
                            }
                            // insert op back for "notify" handling
                            st.ops[user_data] = Some(Operation::Send { id, buf, result: Some(res) });
                            return
                        } else {
                            // reset op reference
                            item.wr_op.take();

                            // release buffer and try to send next chunk
                            if item.ctx.release_write_buf(buf, Poll::Ready(res)).ready() {
                                st.send(id, &self.inner.api);
                            }
                        }
                    }
                }
                Operation::Poll { id } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        if !item.flags.contains(Flags::RD_MORE) && !item.ctx.is_stopped() {
                            item.ctx.stop(res.err());
                        }
                    }
                }
                Operation::Shutdown { tx } => {
                    if let Some(tx) = tx {
                        let _ = tx.send(res.map(|_| ()));
                    }
                }
                Operation::Close { id } => {
                    if st.streams[id].flags.contains(Flags::DROPPED_SEC) {
                        let item = st.streams.remove(id);
                        mem::forget(item.io);
                    } else {
                        st.streams[id].flags.insert(Flags::DROPPED_PRI);
                    }
                }
                Operation::Nop => {}
            }
            let _ = st.ops.remove(user_data);
        });
    }

    fn tick(&mut self) {
        self.inner.check_delayed_feed();
    }

    fn cleanup(&mut self) {
        if let Some(v) = self.inner.storage.take() {
            for (_, val) in v.streams.into_iter() {
                if val.flags.contains(Flags::DROPPED_PRI) {
                    mem::forget(val.io);
                } else {
                    log::trace!(
                        "{}: Unclosed sockets {:?}",
                        val.ctx.tag(),
                        val.io.peer_addr()
                    );
                }
            }
        }
        self.inner.delayed_feed.take();
    }
}

impl StreamOpsStorage {
    fn recv(&mut self, id: usize, poll_first: bool, api: &DriverApi) {
        if let Some(item) = self.streams.get_mut(id) {
            if item.rd_op.is_none() {
                let mut buf = item.ctx.get_read_buf();
                let s = buf.chunk_mut();
                let buf_ptr = s.as_mut_ptr();
                let buf_len = s.len() as u32;
                let op_id = self.ops.insert(Some(Operation::Recv { id, buf })) as u32;
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
    }

    fn recv_more(&mut self, id: usize, mut buf: BytesVec, api: &DriverApi) {
        if let Some(item) = self.streams.get_mut(id) {
            item.ctx.resize_read_buf(&mut buf);

            let slice = buf.chunk_mut();
            let buf_ptr = slice.as_mut_ptr();
            let buf_len = slice.len() as u32;
            let op_id = self.ops.insert(Some(Operation::Recv { id, buf })) as u32;
            item.rd_op = NonZeroU32::new(op_id);

            api.submit_inline(op_id, move |entry| {
                opcode2::Recv::with(entry, item.fd()).buffer(buf_ptr, buf_len);
            });
        }
    }

    fn send(&mut self, id: usize, api: &DriverApi) {
        if let Some(item) = self.streams.get_mut(id) {
            if item.wr_op.is_none() {
                if let Some(buf) = item.ctx.get_write_buf() {
                    let slice = buf.chunk();
                    let buf_ptr = slice.as_ptr();
                    let buf_len = slice.len() as u32;
                    let op_id = self.ops.insert(Some(Operation::Send {
                        id,
                        buf,
                        result: None,
                    })) as u32;
                    item.wr_op = NonZeroU32::new(op_id);

                    api.submit_inline(op_id, move |entry| {
                        if item.flags.contains(Flags::NO_ZC) || buf_len <= ZC_SIZE {
                            opcode2::Send::with(entry, item.fd()).buffer(buf_ptr, buf_len);
                        } else {
                            opcode2::SendZc::with(entry, item.fd())
                                .buffer(buf_ptr, buf_len);
                        }
                    });
                }
            } else if item.flags.contains(Flags::WR_CANCELING) {
                item.flags.insert(Flags::WR_REISSUE);
            }
        }
    }

    fn add_operation(&mut self, op: Operation) -> u32 {
        self.ops.insert(Some(op)) as u32
    }

    fn pause_read(&mut self, id: usize, api: &DriverApi) {
        let item = &mut self.streams[id];
        if let Some(rd_op) = item.rd_op {
            if !item.flags.contains(Flags::RD_CANCELING) {
                item.flags.insert(Flags::RD_CANCELING);
                api.cancel(rd_op.get());
                log::trace!("{}: Recv to pause ({:?})", item.tag(), item.fd());
            }
        }
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

    fn drop_stream(&self, id: usize) {
        // Dropping while `StreamOps` handling event
        if let Some(mut storage) = self.storage.take() {
            let item = &mut storage.streams[id];
            log::trace!("{}: Close ({:?})", item.tag(), item.fd());

            let entry = opcode::Close::new(item.fd()).build();
            let op_id = storage.add_operation(Operation::Close { id });
            self.api.submit(op_id, entry);
            self.storage.set(Some(storage));
        } else {
            self.add_delayed_drop(IdType::Stream(id as u32));
        }
    }

    fn drop_weak_stream(&self, id: usize) {
        // Dropping while `StreamOps` handling event
        if let Some(mut storage) = self.storage.take() {
            let item = &mut storage.streams[id];
            if item.flags.contains(Flags::DROPPED_PRI) {
                // io is closed already, remove from storage
                let item = storage.streams.remove(id);
                mem::forget(item.io);
            } else {
                item.flags.insert(Flags::DROPPED_SEC);
            }
            self.storage.set(Some(storage));
        } else {
            self.add_delayed_drop(IdType::Weak(id as u32));
        }
    }

    fn add_delayed_drop(&self, id: IdType) {
        self.delayed.set(true);
        if let Some(mut feed) = self.delayed_feed.take() {
            feed.push(id);
            self.delayed_feed.set(Some(feed));
        }
    }

    fn check_delayed_feed(&self) {
        if self.delayed.get() {
            self.delayed.set(false);
            if let Some(mut feed) = self.delayed_feed.take() {
                for id in feed.drain(..) {
                    match id {
                        IdType::Stream(id) => self.drop_stream(id as usize),
                        IdType::Weak(id) => self.drop_weak_stream(id as usize),
                    }
                }
                self.delayed_feed.set(Some(feed));
            }
        }
    }
}

impl StreamItem {
    fn fd(&self) -> Fd {
        Fd(self.io.as_raw_fd())
    }

    fn tag(&self) -> &'static str {
        self.ctx.tag()
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self) -> io::Result<()> {
        self.inner
            .with(|storage| {
                storage.pause_read(self.id, &self.inner.api);
                let fd = storage.streams[self.id].fd();
                let (tx, rx) = self.inner.pool.channel();
                let op_id = storage.add_operation(Operation::shutdown(tx));
                self.inner
                    .api
                    .submit(op_id, opcode::Shutdown::new(fd, libc::SHUT_RDWR).build());
                rx
            })
            .await
            .map_err(|_| io::Error::other("gone"))
            .and_then(|item| item)
    }

    pub(crate) fn resume_read(&self) {
        self.inner
            .with(|st| st.recv(self.id, false, &self.inner.api))
    }

    pub(crate) fn resume_write(&self) {
        self.inner.with(|st| st.send(self.id, &self.inner.api))
    }

    pub(crate) fn pause_read(&self) {
        self.inner
            .with(|storage| storage.pause_read(self.id, &self.inner.api))
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        self.inner.drop_stream(self.id);
    }
}

impl WeakStreamCtl {
    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|storage| f(&storage.streams[self.id].io))
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        self.inner.drop_weak_stream(self.id);
    }
}
