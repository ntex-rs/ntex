use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::{cell::Cell, io, mem, num::NonZeroU32, rc::Rc, task::Poll};

use ntex_bytes::{BufMut, BytePage, BytePages, BytesMut};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_io_uring::{cqueue, opcode, opcode2, types::Fd};
use ntex_rt::Arbiter;
use ntex_util::channel::pool;
use slab::Slab;
use socket2::Socket;

use super::reactor::{Handler, Reactor, ReactorApi};
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
    struct Flags: u16 {
        const RD_CANCELING = 0b0000_0000_0001;
        const RD_REISSUE   = 0b0000_0000_0010;
        const RD_MORE      = 0b0000_0000_0100;
        const WR_CANCELING = 0b0000_0000_1000;
        const WR_REISSUE   = 0b0000_0001_0000;
        const NO_ZC        = 0b0000_0010_0000;
        const DROPPED_PRI  = 0b0000_0100_0000;
        const DROPPED_SEC  = 0b0000_1000_0000;
        /// `Close` is submitted, descriptor belongs to the kernel
        const CLOSING      = 0b0001_0000_0000;
        /// `POLLHUP` fired while `RD_MORE` reads were in progress
        const HUP_DEFERRED = 0b0010_0000_0000;
        /// Descriptor is registered in the `HupWatcher` epoll instance
        const WATCHED      = 0b0100_0000_0000;
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
    poll_op: Option<NonZeroU32>,
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
    /// Readiness of the `HupWatcher` epoll instance
    Watch,
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
    api: ReactorApi,
    delayed_feed: Queue<IdType>,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    pool: pool::Pool<io::Result<()>>,
    default_flags: Flags,
}

struct StreamOpsStorage {
    ops: Slab<Option<Operation>>,
    streams: Slab<StreamItem>,
    watcher: HupWatcher,
}

/// Watches half-closed sockets for `POLLHUP` and `POLLERR`.
///
/// io-uring always adds `POLLRDHUP` to the poll mask, so after the peer
/// half-closes the connection, a socket poll completes immediately and cannot
/// be used to wait for terminal conditions. epoll reports only requested
/// events plus `EPOLLHUP` and `EPOLLERR`, so half-closed sockets are moved
/// to an epoll instance, which is in turn polled through io-uring.
#[derive(Default)]
struct HupWatcher {
    epoll: Option<OwnedFd>,
    op: Option<NonZeroU32>,
    count: usize,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(reactor: &Reactor) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            reactor.register(|api| {
                let default_flags = if api.is_supported(opcode::SendZc::CODE) {
                    Flags::empty()
                } else {
                    Flags::NO_ZC
                };
                assert!(
                    api.is_supported(opcode::Close::CODE),
                    "opcode::Close is required for io-uring support"
                );
                assert!(
                    api.is_supported(opcode::Shutdown::CODE),
                    "opcode::Shutdown is required for io-uring support"
                );

                let mut ops = Slab::new();
                ops.insert(Some(Operation::Nop));

                let ops = Rc::new(StreamOpsInner {
                    api,
                    default_flags,
                    delayed_feed: Queue::new(),
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        ops,
                        streams: Slab::new(),
                        watcher: HupWatcher::default(),
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
            poll_op: None,
            flags: if zc { self.0.default_flags } else { Flags::NO_ZC },
        };

        let id = self.0.with(|st| {
            let id = st.streams.insert(item);
            st.arm_poll(id, &self.0.api);
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
                        #[cfg(feature = "trace")]
                        log::trace!("{}: Recv canceled {:?}", item.tag(), item.fd());
                        item.rd_op.take();
                        item.flags.remove(Flags::RD_CANCELING);

                        let res = item.ctx.release_read_buf(buf, Poll::Pending);
                        if item.flags.contains(Flags::RD_REISSUE) || res == IoTaskStatus::Io {
                            item.flags.remove(Flags::RD_REISSUE);
                            st.recv(id, false, &self.inner.api);
                        }
                    }
                }
                Operation::Send { id, buf, .. } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        #[cfg(feature = "trace")]
                        log::trace!("{}: Send canceled: {:?}", item.tag(), item.fd());
                        item.ctx.with_write_dst(|pages| pages.prepend(buf));
                        item.wr_op.take();
                        item.flags.remove(Flags::WR_CANCELING);

                        let res = item.ctx.update_write_status(Ok(0));
                        if item.flags.contains(Flags::WR_REISSUE) || res == IoTaskStatus::Io {
                            item.flags.remove(Flags::WR_REISSUE);
                            st.send(id, &self.inner.api);
                        }
                    }
                }
                Operation::Poll { id } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        item.poll_op.take();
                    }
                }
                Operation::Watch => {
                    st.watcher.op.take();
                }
                Operation::Nop | Operation::Close { .. } | Operation::Shutdown { .. } => {}
            });
    }

    #[allow(clippy::too_many_lines)]
    fn completed(&mut self, user_data: usize, flags: u32, res: io::Result<usize>) {
        self.inner.with(|st| {
            match st.ops[user_data].take().unwrap() {
                Operation::Recv { id, mut buf, } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        #[cfg(feature = "trace")]
                        log::trace!(
                            "{}: RcvDone({id}) {res:?} non-empty:{}",
                            item.ctx.tag(),
                            cqueue::sock_nonempty(flags));

                        // reset op reference, a pending cancel lost the race
                        // with this completion and has nothing to cancel anymore
                        let _ = item.rd_op.take();
                        item.flags.remove(Flags::RD_CANCELING | Flags::RD_REISSUE);

                        // handle WouldBlock
                        if matches!(res, Err(ref e) if e.kind() == io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)) {
                            log::error!("{}: Received WouldBlock {:?}, id: {:?}", item.tag(), res, item.ctx.id());
                            st.recv_more(id, buf, &self.inner.api);
                        } else {
                            if let Ok(size) = res && size > 0 {
                                // SAFETY: kernel tells us how many bytes it read
                                unsafe { buf.advance_mut(size) };
                            }

                            // handle IORING_CQE_F_SOCK_NONEMPTY flag
                            //
                            // In case of disconnect, sock_nonempty is set to true.
                            // First completion contains data, second Recv(0)
                            // Before receiving Recv(0), POLLHUP can be triggered
                            // Reactor must read all recv() call before handling
                            // disconnects
                            let more = cqueue::sock_nonempty(flags) && !matches!(res, Ok(0) | Err(_));

                            // Input is released on every completion, so that read
                            // back-pressure and pauses stop the chain of reads
                            let status = item.ctx.release_read_buf(buf, Poll::Ready(res));
                            if more && status == IoTaskStatus::Io {
                                item.flags.insert(Flags::RD_MORE);
                                st.recv(id, false, &self.inner.api);
                            } else {
                                item.flags.remove(Flags::RD_MORE);
                                if !more && item.flags.contains(Flags::HUP_DEFERRED) {
                                    // input is drained, handle deferred disconnect
                                    item.flags.remove(Flags::HUP_DEFERRED);
                                    item.ctx.stop(None);
                                } else if status == IoTaskStatus::Io {
                                    st.recv(id, self.inner.api.is_new(), &self.inner.api);
                                }
                            }
                        }
                    }
                }
                Operation::Send { id, buf, result } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        #[cfg(feature = "trace")]
                        log::trace!(
                            "{}: Sent({id}) res:{res:?} notif:{:?} more:{:?}",
                            item.ctx.tag(),
                            cqueue::notif(flags),
                            cqueue::more(flags),
                        );

                        if cqueue::notif(flags) {
                            let res = result.unwrap_or(res);
                            let res = complete_send(&item.ctx, buf, res);
                            if item.ctx.update_write_status(res) == IoTaskStatus::Io {
                                st.send(id, &self.inner.api);
                            }
                        } else if cqueue::more(flags) {
                            // reset op reference
                            item.wr_op.take();

                            // try to send next chunk
                            if matches!(&res, Ok(n) if *n > 0) {
                                st.send(id, &self.inner.api);
                            }
                            // insert op back for "notify" handling
                            st.ops[user_data] = Some(Operation::Send {
                                id,
                                buf,
                                result: Some(res) });
                            // we reuse same op id
                            return
                        } else {
                            // reset op reference
                            item.wr_op.take();

                            // release buffer and try to send next chunk
                            let res = complete_send(&item.ctx, buf, res);
                            if item.ctx.update_write_status(res) == IoTaskStatus::Io {
                                st.send(id, &self.inner.api);
                            }
                        }
                    }
                }
                Operation::Poll { id } => {
                    if let Some(item) = st.streams.get_mut(id) {
                        item.poll_op.take();
                        #[allow(clippy::cast_sign_loss)]
                        match res {
                            Ok(ev) if ev & libc::POLLERR as usize != 0 => item.error(),
                            Ok(ev) if ev & libc::POLLHUP as usize != 0 => item.hangup(),
                            // `POLLRDHUP`, peer half-closed the connection.
                            // Re-arming would complete immediately again
                            Ok(_) => st.watch(id, &self.inner.api),
                            Err(e) => item.ctx.stop(Some(e)),
                        }
                    }
                }
                Operation::Watch => {
                    st.watcher.op.take();
                    if res.is_ok() {
                        st.watch_completed(&self.inner.api);
                    } else {
                        log::error!("HUP watcher poll failed: {res:?}");
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
                        #[cfg(feature = "trace")]
                        log::trace!("{}: Close({id})", item.ctx.tag());
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
        // Reactor has flushed pending submissions and canceled all in-flight
        // operations. Release every socket here, otherwise stored `IoContext`s
        // keep `StreamOpsInner` alive through the io handle and nothing closes.
        if let Some(mut v) = self.inner.storage.take() {
            for item in v.streams.drain() {
                if item.flags.intersects(Flags::DROPPED_PRI | Flags::CLOSING) {
                    // descriptor is closed or being closed by the kernel
                    mem::forget(item.io);
                } else {
                    log::trace!(
                        "{}: Unclosed socket {:?}",
                        item.ctx.tag(),
                        item.io.peer_addr()
                    );
                    drop(item.io);
                }
            }
            v.watcher = HupWatcher::default();
            self.inner.storage.set(Some(v));
        }
        self.inner.delayed_feed.clear();
    }
}

fn write_status(n: usize) -> io::Result<usize> {
    if n == 0 {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write frame to transport",
        ))
    } else {
        Ok(n)
    }
}

/// Completes a send, returning output that did not reach the peer.
///
/// The page was taken out of the write buffer when the send was submitted, so
/// it is counted as in-flight output. Whatever the kernel did not accept has
/// to go back, otherwise it is both lost and left counted as outstanding.
fn complete_send(ctx: &IoContext, mut buf: BytePage, res: io::Result<usize>) -> io::Result<usize> {
    if let Ok(n) = res
        && n < buf.len()
    {
        buf.advance_to(n);
        ctx.with_write_dst(|pages| pages.prepend(buf));
    }
    res.and_then(write_status)
}

impl StreamOpsStorage {
    fn recv(&mut self, id: usize, poll_first: bool, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
            if item.rd_op.is_none() {
                #[cfg(feature = "trace")]
                log::trace!("{}: Rcv({id})", item.ctx.tag());

                let mut buf = item.ctx.take_read_buf();
                let s = buf.chunk_mut();
                let buf_ptr = s.as_mut_ptr();
                let buf_len = s.len() as u32;
                let op_id = self.ops.insert(Some(Operation::Recv { id, buf })) as u32;
                item.rd_op = NonZeroU32::new(op_id);

                api.submit_inline(op_id, move |entry| {
                    let op = opcode2::Recv::with(entry, item.fd()).buffer(buf_ptr, buf_len);
                    if poll_first {
                        op.ioprio(IORING_RECVSEND_POLL_FIRST);
                    }
                });
            } else if item.flags.contains(Flags::RD_CANCELING) {
                item.flags.insert(Flags::RD_REISSUE);
            }
        }
    }

    fn recv_more(&mut self, id: usize, mut buf: BytesMut, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
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

    fn send(&mut self, id: usize, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
            if item.wr_op.is_none() {
                let page = item.ctx.with_write_dst(BytePages::take);
                if let Some(buf) = page {
                    #[cfg(feature = "trace")]
                    log::trace!("{}: Snd({id}) size:{:?}", item.ctx.tag(), buf.len());

                    let op_id = self.ops.insert(Some(Operation::Send {
                        id,
                        buf,
                        result: None,
                    })) as u32;
                    item.wr_op = NonZeroU32::new(op_id);

                    let (buf_ptr, buf_len) =
                        if let Some(Operation::Send { buf, .. }) = &self.ops[op_id as usize] {
                            // Safety. `buf` is stored in `self.ops` which is heap.
                            (unsafe { buf.as_ptr() }, buf.len() as u32)
                        } else {
                            unreachable!()
                        };

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
    }

    /// Watch for terminal conditions, `POLLHUP` and `POLLERR`
    fn arm_poll(&mut self, id: usize, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && item.poll_op.is_none()
            && !item.flags.contains(Flags::CLOSING)
        {
            let op =
                opcode::PollAdd::new(item.fd(), (libc::POLLHUP | libc::POLLERR) as u32).build();
            let op_id = self.ops.insert(Some(Operation::Poll { id })) as u32;
            item.poll_op = NonZeroU32::new(op_id);
            api.submit(op_id, op);
        }
    }

    fn add_operation(&mut self, op: Operation) -> u32 {
        self.ops.insert(Some(op)) as u32
    }

    /// Move half-closed socket to the `HupWatcher`
    fn watch(&mut self, id: usize, api: &ReactorApi) {
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        if item.flags.intersects(Flags::WATCHED | Flags::CLOSING) {
            return;
        }
        let epoll = if let Some(ref epoll) = self.watcher.epoll {
            epoll
        } else {
            // SAFETY: plain syscall, result is checked
            let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if fd < 0 {
                log::error!("Cannot create HUP watcher: {}", io::Error::last_os_error());
                return;
            }
            // SAFETY: `fd` is a new descriptor owned by nobody else
            self.watcher
                .epoll
                .insert(unsafe { OwnedFd::from_raw_fd(fd) })
        };

        // `EPOLLHUP` and `EPOLLERR` are always reported
        let mut ev = libc::epoll_event {
            events: libc::EPOLLONESHOT as u32,
            u64: id as u64,
        };
        // SAFETY: both descriptors are valid, `ev` outlives the call
        let res = unsafe {
            libc::epoll_ctl(
                epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                item.io.as_raw_fd(),
                &raw mut ev,
            )
        };
        if res < 0 {
            log::error!(
                "{}: Cannot watch half-closed socket: {}",
                item.tag(),
                io::Error::last_os_error()
            );
            return;
        }
        #[cfg(feature = "trace")]
        log::trace!("{}: Watch HUP({id})", item.tag());

        item.flags.insert(Flags::WATCHED);
        self.watcher.count += 1;
        self.arm_watcher(api);
    }

    /// Remove socket from the `HupWatcher`
    fn unwatch(&mut self, id: usize) {
        if let Some(item) = self.streams.get_mut(id)
            && item.flags.contains(Flags::WATCHED)
        {
            item.flags.remove(Flags::WATCHED);
            self.watcher.count -= 1;
            if let Some(ref epoll) = self.watcher.epoll {
                // SAFETY: both descriptors are valid
                unsafe {
                    libc::epoll_ctl(
                        epoll.as_raw_fd(),
                        libc::EPOLL_CTL_DEL,
                        item.io.as_raw_fd(),
                        std::ptr::null_mut(),
                    );
                }
            }
        }
    }

    fn arm_watcher(&mut self, api: &ReactorApi) {
        if self.watcher.op.is_none()
            && self.watcher.count > 0
            && let Some(ref epoll) = self.watcher.epoll
        {
            let op = opcode::PollAdd::new(Fd(epoll.as_raw_fd()), libc::POLLIN as u32).build();
            let op_id = self.add_operation(Operation::Watch);
            self.watcher.op = NonZeroU32::new(op_id);
            api.submit(op_id, op);
        }
    }

    fn watch_completed(&mut self, api: &ReactorApi) {
        const EVENTS: i32 = 64;

        let Some(epfd) = self.watcher.epoll.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; EVENTS as usize];
        loop {
            // SAFETY: `events` is valid for `EVENTS` entries, zero timeout
            let n = unsafe { libc::epoll_wait(epfd, events.as_mut_ptr(), EVENTS, 0) };
            let Ok(n) = usize::try_from(n) else {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                log::error!("HUP watcher failed: {err}");
                break;
            };

            for ev in &events[..n] {
                let (flags, id) = (ev.events, ev.u64);
                #[allow(clippy::cast_possible_truncation)]
                let id = id as usize;
                // registration is one-shot, the event can be reported only once
                self.unwatch(id);

                if let Some(item) = self.streams.get_mut(id)
                    && !item.flags.contains(Flags::CLOSING)
                {
                    #[cfg(feature = "trace")]
                    log::trace!("{}: HUP watcher({id}) {flags:#x}", item.tag());

                    #[allow(clippy::cast_sign_loss)]
                    if flags & libc::EPOLLERR as u32 != 0 {
                        item.error();
                    } else if flags & libc::EPOLLHUP as u32 != 0 {
                        item.hangup();
                    }
                }
            }
            if n < EVENTS as usize {
                break;
            }
        }
        self.arm_watcher(api);
    }

    fn pause_read(&mut self, id: usize, api: &ReactorApi) {
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        if let Some(rd_op) = item.rd_op
            && !item.flags.contains(Flags::RD_CANCELING)
        {
            item.flags.insert(Flags::RD_CANCELING);
            api.cancel(rd_op.get());
            log::trace!("{}: Recv to pause ({:?})", item.tag(), item.fd());
        }
    }

    fn drop_stream(&mut self, id: usize, api: &ReactorApi) {
        // descriptor must leave the epoll set while it is still valid
        self.unwatch(id);

        // Dropping while `StreamOps` handling event
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        log::trace!("{}: Close ({:?})", item.tag(), item.fd());

        item.flags.insert(Flags::CLOSING);

        // `Close` only removes the descriptor from the file table, in-flight
        // operations keep the socket open, so no FIN or RST is sent until
        // they complete. The `POLLHUP` poll in particular completes only
        // once the connection is gone. Operations are canceled by id, which does
        // not depend on the descriptor still being present in the table.
        if let Some(op) = item.rd_op
            && !item.flags.contains(Flags::RD_CANCELING)
        {
            item.flags.insert(Flags::RD_CANCELING);
            api.cancel(op.get());
        }
        if let Some(op) = item.wr_op
            && !item.flags.contains(Flags::WR_CANCELING)
        {
            item.flags.insert(Flags::WR_CANCELING);
            api.cancel(op.get());
        }
        if let Some(op) = item.poll_op {
            api.cancel(op.get());
        }

        let entry = opcode::Close::new(item.fd()).build();
        let op_id = self.add_operation(Operation::Close { id });
        api.submit(op_id, entry);
    }

    fn drop_weak_stream(&mut self, id: usize) {
        // Dropping while `StreamOps` handling event
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        if item.flags.contains(Flags::DROPPED_PRI) {
            // io is closed already, remove from storage
            let item = self.streams.remove(id);
            mem::forget(item.io);
        } else {
            item.flags.insert(Flags::DROPPED_SEC);
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

    fn check_delayed_feed(&self) {
        if let Some(mut storage) = self.storage.take() {
            while let Some(id) = self.delayed_feed.pop() {
                match id {
                    IdType::Stream(id) => storage.drop_stream(id as usize, &self.api),
                    IdType::Weak(id) => storage.drop_weak_stream(id as usize),
                }
            }
            self.storage.set(Some(storage));
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

    /// Socket reported `POLLERR`
    fn error(&mut self) {
        let err = self.io.take_error().ok().flatten();
        self.ctx.stop(err);
    }

    /// Socket reported `POLLHUP`
    fn hangup(&mut self) {
        if self.flags.contains(Flags::RD_MORE) {
            // Notification is one-shot, remember the
            // disconnect until pending input is read
            self.flags.insert(Flags::HUP_DEFERRED);
        } else {
            self.ctx.stop(None);
        }
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self, terminate: bool) -> io::Result<()> {
        if terminate {
            // The connection was force-closed, so it is released without the
            // graceful close: the receive queue is not drained and no
            // `SHUT_RDWR` is submitted. The socket is aborted instead, so that
            // the peer sees an RST and cannot mistake a truncated stream for a
            // complete one. Dropping this handle submits the `Close` that
            // releases the descriptor.
            self.inner.with(|storage| {
                storage.pause_read(self.id, &self.inner.api);
                crate::helpers::abort_raw_socket(storage.streams[self.id].fd().0);
            });
            return Ok(());
        }

        self.inner
            .with(|storage| {
                storage.pause_read(self.id, &self.inner.api);
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: Shutdown ({:?})",
                    storage.streams[self.id].ctx.tag(),
                    self.id
                );
                let fd = storage.streams[self.id].fd();
                if storage.streams[self.id].rd_op.is_none() {
                    // `pause_read()` above only submits a cancel, so a recv can
                    // still be in flight; draining now would race it. Nothing is
                    // lost by skipping, that recv empties the queue itself.
                    crate::helpers::drain_raw_socket(fd.0);
                }
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
            .with(|st| st.recv(self.id, false, &self.inner.api));
    }

    pub(crate) fn resume_write(&self) {
        self.inner.with(|st| st.send(self.id, &self.inner.api));
    }

    pub(crate) fn pause_read(&self) {
        self.inner
            .with(|storage| storage.pause_read(self.id, &self.inner.api));
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        if let Some(mut storage) = self.inner.storage.take() {
            storage.drop_stream(self.id, &self.inner.api);
            self.inner.storage.set(Some(storage));
        } else {
            self.inner.delayed_feed.push(IdType::Stream(self.id as u32));
        }
    }
}

impl WeakStreamCtl {
    pub(crate) fn with_io<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner
            .with(|storage| storage.streams.get(self.id).map(|item| f(&item.io)))
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        if let Some(mut storage) = self.inner.storage.take() {
            storage.drop_weak_stream(self.id);
            self.inner.storage.set(Some(storage));
        } else {
            self.inner.delayed_feed.push(IdType::Weak(self.id as u32));
        }
    }
}
