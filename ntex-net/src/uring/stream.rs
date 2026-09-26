use std::{cell::Cell, io, mem, num::NonZeroU32, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::{BufMut, BytePage, BytePages, Bytes, BytesMut, info::PageKind};
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
        const WR_CANCELING = 0b0000_0000_1000;
        const WR_REISSUE   = 0b0000_0001_0000;
        const NO_ZC        = 0b0000_0010_0000;
        const DROPPED_PRI  = 0b0000_0100_0000;
        const DROPPED_SEC  = 0b0000_1000_0000;
        /// `Close` is submitted, descriptor belongs to the kernel
        const CLOSING      = 0b0001_0000_0000;
        /// `SendMsg` is not supported, send one page at a time
        const NO_MSG       = 0b0010_0000_0000;
        /// `SendMsgZc` is not supported
        const NO_ZC_MSG    = 0b0100_0000_0000;
    }
}

/// Smaller sends are copied, zero-copy setup costs more than it saves
const ZC_SIZE: u32 = 16 * 1024;
const ZC_MAX_SIZE: u32 = 128 * 1024;
/// Limits for the pages gathered into one send
const SEND_MAX_PAGES: usize = 16;
const SEND_MAX_SIZE: usize = 256 * 1024;
const IORING_RECVSEND_POLL_FIRST: u16 = 1;
/// Ask the kernel to report in the notification whether data was copied
const IORING_SEND_ZC_REPORT_USAGE: u16 = 8;
const IORING_NOTIF_USAGE_ZC_COPIED: usize = 1 << 31;

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
        buf: BytesMut,
    },
    /// Send of gathered pages, or of an inline page
    Send {
        id: usize,
        buf: Box<SendBuf>,
        state: SendState,
    },
    /// Send of a single heap backed page, no allocation required
    SendOne {
        id: usize,
        page: BytePage,
        state: SendState,
    },
    Shutdown {
        tx: Option<pool::Sender<io::Result<()>>>,
    },
    Close {
        id: usize,
    },
    Nop,
}

/// Pages of one send operation.
///
/// Boxed, the kernel references the pages, the `iovec` array and the `msghdr`
/// by address until the operation, or its zero-copy notification, completes.
/// Pages are stored from the start of the array without gaps.
struct SendBuf {
    pages: [Option<BytePage>; SEND_MAX_PAGES],
    iov: [libc::iovec; SEND_MAX_PAGES],
    msg: libc::msghdr,
}

#[derive(Debug)]
struct SendState {
    /// Result of the first completion of a zero-copy send
    result: Option<io::Result<usize>>,
    /// Zero-copy send
    zc: bool,
    /// Zero-copy send with `IORING_SEND_ZC_REPORT_USAGE`
    zc_report: bool,
    /// Output was already returned to the write buffer for a resend
    resent: bool,
}

impl Operation {
    /// Splits `Send` and `SendOne` operations for shared completion handling
    fn into_send(self) -> (usize, SendData, SendState) {
        match self {
            Operation::Send { id, buf, state } => (id, SendData::Pages(buf), state),
            Operation::SendOne { id, page, state } => (id, SendData::Page(page), state),
            _ => unreachable!("not a send operation"),
        }
    }
}

/// Output of one send operation.
///
/// A single page is stored in the operation itself, the kernel references
/// its heap data, which does not move with the operations slab. Inline pages
/// and gathered pages are boxed.
#[derive(Debug)]
enum SendData {
    Page(BytePage),
    Pages(Box<SendBuf>),
}

/// Zero-copy send failure that is resolved by resending the output
#[derive(Copy, Clone, Debug)]
enum ZcRetry {
    /// The kernel rejected `IORING_SEND_ZC_REPORT_USAGE`
    NoReport,
    /// Pinned pages are charged against `RLIMIT_MEMLOCK` until the
    /// notification arrives, the limit is shared by all connections
    NoMem,
}

impl ZcRetry {
    fn check(state: &SendState, res: &io::Result<usize>) -> Option<Self> {
        match res.as_ref().err()?.raw_os_error()? {
            libc::EINVAL if state.zc_report => Some(ZcRetry::NoReport),
            libc::ENOMEM | libc::ENOBUFS if state.zc => Some(ZcRetry::NoMem),
            _ => None,
        }
    }

    fn apply(self, item: &mut StreamItem, zc_report: &mut bool) {
        match self {
            ZcRetry::NoReport => {
                log::debug!(
                    "{}: IORING_SEND_ZC_REPORT_USAGE is not supported",
                    item.tag()
                );
                *zc_report = false;
            }
            ZcRetry::NoMem => {
                log::debug!(
                    "{}: Zero-copy send failed with no memory, disable ({:?})",
                    item.tag(),
                    item.fd()
                );
                item.flags.insert(Flags::NO_ZC);
            }
        }
    }
}

/// Output of `page` past `offset` that shares the page's data, or a copy for
/// `Vec` pages whose split would copy and free the original.
fn shared_tail(page: &BytePage, offset: usize) -> BytePage {
    if page.info() == PageKind::Vec {
        BytePage::from(Bytes::copy_from_slice(&page[offset..]))
    } else {
        // `freeze` gives a view of its own, advancing a shared `BytesMut`
        // storage in place would move the start of the original page too
        let mut p = BytePage::from(page.clone().freeze());
        p.advance_to(offset);
        p
    }
}

impl SendData {
    /// Takes output for one send, gathering up to `max_pages` pages and
    /// `max_size` bytes.
    fn take(dst: &mut BytePages, max_pages: usize, max_size: usize) -> Option<Self> {
        let first = dst.take()?;
        let second = if max_pages > 1 { dst.take() } else { None };
        let second = match second {
            Some(page) if first.len() + page.len() <= max_size => page,
            second => {
                if let Some(page) = second {
                    dst.prepend(page);
                }
                // the kernel references the page data by address
                // while the operation moves with the slab
                if !first.is_inline() {
                    return Some(SendData::Page(first));
                }
                let mut buf = SendBuf::new();
                buf.pages[0] = Some(first);
                return Some(SendData::Pages(buf));
            }
        };
        let mut size = first.len() + second.len();
        let mut buf = SendBuf::new();
        buf.pages[0] = Some(first);
        buf.pages[1] = Some(second);
        for slot in &mut buf.pages[2..max_pages] {
            let Some(page) = dst.take() else { break };
            if size + page.len() > max_size {
                dst.prepend(page);
                break;
            }
            size += page.len();
            *slot = Some(page);
        }
        Some(SendData::Pages(buf))
    }

    fn into_op(self, id: usize, state: SendState) -> Operation {
        match self {
            SendData::Page(page) => Operation::SendOne { id, page, state },
            SendData::Pages(buf) => Operation::Send { id, buf, state },
        }
    }

    fn len(&self) -> usize {
        match self {
            SendData::Page(page) => page.len(),
            SendData::Pages(buf) => buf.len(),
        }
    }

    /// Returns output past `sent` to the write buffer, the kernel is done
    /// with the pages.
    fn release(self, ctx: &IoContext, sent: usize) {
        match self {
            SendData::Page(mut page) => {
                if sent < page.len() {
                    page.advance_to(sent);
                    ctx.with_write_dst(|dst| dst.prepend(page));
                }
            }
            SendData::Pages(buf) => buf.release(ctx, sent),
        }
    }

    /// Returns output past `sent` to the write buffer while the kernel still
    /// references the pages until the zero-copy notification arrives.
    fn release_shared(&self, ctx: &IoContext, sent: usize) {
        match self {
            SendData::Page(page) => {
                if sent < page.len() {
                    ctx.with_write_dst(|dst| dst.prepend(shared_tail(page, sent)));
                }
            }
            SendData::Pages(buf) => buf.release_shared(ctx, sent),
        }
    }
}

impl std::fmt::Debug for SendBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendBuf")
            .field("pages", &self.pages().count())
            .field("len", &self.len())
            .finish()
    }
}

impl SendBuf {
    fn new() -> Box<Self> {
        Box::new(SendBuf {
            pages: [const { None }; SEND_MAX_PAGES],
            iov: [libc::iovec {
                iov_base: std::ptr::null_mut(),
                iov_len: 0,
            }; SEND_MAX_PAGES],
            // SAFETY: all-zero is a valid `msghdr`
            msg: unsafe { mem::zeroed() },
        })
    }

    fn pages(&self) -> impl Iterator<Item = &BytePage> {
        self.pages.iter().map_while(Option::as_ref)
    }

    fn len(&self) -> usize {
        self.pages().map(BytePage::len).sum()
    }

    /// Position of the first byte past `sent`, as page index and offset.
    fn unsent(&self, mut sent: usize) -> Option<(usize, usize)> {
        for (idx, page) in self.pages().enumerate() {
            if sent < page.len() {
                return Some((idx, sent));
            }
            sent -= page.len();
        }
        None
    }

    /// Returns output past `sent` to the write buffer, the kernel is done
    /// with the pages.
    fn release(mut self: Box<Self>, ctx: &IoContext, sent: usize) {
        if let Some((idx, offset)) = self.unsent(sent) {
            ctx.with_write_dst(|dst| {
                for i in (idx..SEND_MAX_PAGES).rev() {
                    if let Some(mut page) = self.pages[i].take() {
                        if i == idx {
                            page.advance_to(offset);
                        }
                        dst.prepend(page);
                    }
                }
            });
        }
    }

    /// Returns output past `sent` to the write buffer while the kernel still
    /// references the pages until the zero-copy notification arrives.
    fn release_shared(&self, ctx: &IoContext, sent: usize) {
        if let Some((idx, offset)) = self.unsent(sent) {
            ctx.with_write_dst(|dst| {
                for i in (idx..SEND_MAX_PAGES).rev() {
                    if let Some(page) = &self.pages[i] {
                        let offset = if i == idx { offset } else { 0 };
                        dst.prepend(shared_tail(page, offset));
                    }
                }
            });
        }
    }
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
    /// `IORING_SEND_ZC_REPORT_USAGE` is supported, cleared on first `EINVAL`
    zc_report: bool,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(reactor: &Reactor) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            reactor.register(|api| {
                let mut default_flags = if api.is_supported(opcode::SendZc::CODE) {
                    Flags::empty()
                } else {
                    Flags::NO_ZC
                };
                if !api.is_supported(opcode::SendMsg::CODE) {
                    default_flags.insert(Flags::NO_MSG);
                }
                if !api.is_supported(opcode::SendMsgZc::CODE) {
                    default_flags.insert(Flags::NO_ZC_MSG);
                }
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
                        zc_report: true,
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
            flags: if zc {
                self.0.default_flags
            } else {
                self.0.default_flags | Flags::NO_ZC
            },
        };

        let id = self.0.with(|st| st.streams.insert(item));
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
                op @ (Operation::Send { .. } | Operation::SendOne { .. }) => {
                    let (id, buf, _) = op.into_send();
                    if let Some(item) = st.streams.get_mut(id) {
                        #[cfg(feature = "trace")]
                        log::trace!("{}: Send canceled: {:?}", item.tag(), item.fd());
                        buf.release(&item.ctx, 0);
                        item.wr_op.take();
                        item.flags.remove(Flags::WR_CANCELING);

                        let res = item.ctx.update_write_status(Ok(0));
                        if item.flags.contains(Flags::WR_REISSUE) || res == IoTaskStatus::Io {
                            item.flags.remove(Flags::WR_REISSUE);
                            st.send(id, &self.inner.api);
                        }
                    }
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
                            st.recv_more(id, buf, true, &self.inner.api);
                        } else {
                            if let Ok(size) = res && size > 0 {
                                // SAFETY: kernel tells us how many bytes it read
                                unsafe { buf.advance_mut(size) };
                            }

                            // handle IORING_CQE_F_SOCK_NONEMPTY flag, more input
                            // is queued, keep reading into the same buffer until
                            // it is full. The buffer is not grown, so read
                            // backpressure applies once it is released
                            if cqueue::sock_nonempty(flags)
                                && buf.remaining_mut() > 0
                                && !matches!(res, Ok(0) | Err(_))
                            {
                                st.recv_more(id, buf, false, &self.inner.api);
                            } else if item.ctx.release_read_buf(buf, Poll::Ready(res))
                                == IoTaskStatus::Io
                            {
                                st.recv(id, self.inner.api.is_new(), &self.inner.api);
                            }
                        }
                    }
                }
                op @ (Operation::Send { .. } | Operation::SendOne { .. }) => {
                    let (id, buf, mut state) = op.into_send();
                    if let Some(item) = st.streams.get_mut(id) {
                        #[cfg(feature = "trace")]
                        log::trace!(
                            "{}: Sent({id}) res:{res:?} notif:{:?} more:{:?}",
                            item.ctx.tag(),
                            cqueue::notif(flags),
                            cqueue::more(flags),
                        );

                        if cqueue::notif(flags) {
                            // the kernel copied data anyway (loopback, veth, no
                            // scatter-gather), zero-copy only adds overhead
                            if state.zc_report
                                && matches!(res, Ok(v) if v & IORING_NOTIF_USAGE_ZC_COPIED != 0)
                                && !item.flags.contains(Flags::NO_ZC)
                            {
                                #[cfg(feature = "trace")]
                                log::trace!("{}: Zero-copy send was copied, disable ({:?})", item.tag(), item.fd());
                                item.flags.insert(Flags::NO_ZC);
                            }
                            if state.resent {
                                let _ = st.ops.remove(user_data);
                                return;
                            }
                            let has_result = state.result.is_some();
                            let res = state.result.unwrap_or(res);
                            if matches!(res, Err(ref e) if e.raw_os_error() == Some(libc::ECANCELED)) {
                                #[cfg(feature = "trace")]
                                log::trace!("{}: Send canceled: {:?}", item.tag(), item.fd());
                                buf.release(&item.ctx, 0);
                                item.flags.remove(Flags::WR_CANCELING);

                                let res = item.ctx.update_write_status(Ok(0));
                                if item.flags.contains(Flags::WR_REISSUE) || res == IoTaskStatus::Io {
                                    item.flags.remove(Flags::WR_REISSUE);
                                    st.send(id, &self.inner.api);
                                }
                                let _ = st.ops.remove(user_data);
                                return;
                            }
                            // unsent output was returned with the first completion
                            let res = if has_result {
                                res.and_then(write_status)
                            } else {
                                complete_send(&item.ctx, buf, res)
                            };
                            if item.ctx.update_write_status(res) == IoTaskStatus::Io {
                                st.send(id, &self.inner.api);
                            }
                        } else if cqueue::more(flags) {
                            // reset op reference
                            item.wr_op.take();

                            if let Ok(n) = res && n > 0 {
                                // Unsent output has to go back before the next
                                // chunk is sent to keep output in order
                                if n < buf.len() {
                                    buf.release_shared(&item.ctx, n);
                                }
                                st.send(id, &self.inner.api);
                            } else if let Some(retry) = ZcRetry::check(&state, &res) {
                                retry.apply(item, &mut st.zc_report);
                                // the kernel may hold pages until the notification
                                buf.release_shared(&item.ctx, 0);
                                state.resent = true;
                                st.send(id, &self.inner.api);
                            }
                            // insert op back for "notify" handling
                            state.result = Some(res);
                            st.ops[user_data] = Some(buf.into_op(id, state));
                            // we reuse same op id
                            return
                        } else {
                            // reset op reference
                            item.wr_op.take();

                            // resend without the failed zero-copy feature
                            if let Some(retry) = ZcRetry::check(&state, &res) {
                                retry.apply(item, &mut st.zc_report);
                                buf.release(&item.ctx, 0);
                                st.send(id, &self.inner.api);
                                let _ = st.ops.remove(user_data);
                                return;
                            }

                            // release buffer and try to send next chunk
                            let res = complete_send(&item.ctx, buf, res);
                            if item.ctx.update_write_status(res) == IoTaskStatus::Io {
                                st.send(id, &self.inner.api);
                            }
                        }
                    } else if cqueue::more(flags) && !cqueue::notif(flags) {
                        // stream is gone, but the kernel still holds the buffer
                        // until the zero-copy notification arrives
                        st.ops[user_data] = Some(buf.into_op(id, state));
                        return;
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
/// The pages were taken out of the write buffer when the send was submitted,
/// so they are counted as in-flight output. Whatever the kernel did not accept
/// has to go back, otherwise it is both lost and left counted as outstanding.
fn complete_send(ctx: &IoContext, buf: SendData, res: io::Result<usize>) -> io::Result<usize> {
    if let Ok(n) = res
        && n < buf.len()
    {
        buf.release(ctx, n);
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

    fn recv_more(&mut self, id: usize, mut buf: BytesMut, resize: bool, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
            if resize {
                item.ctx.resize_read_buf(&mut buf);
            }

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
                let zc = !item.flags.contains(Flags::NO_ZC);
                let max_pages = if item.flags.contains(Flags::NO_MSG) {
                    1
                } else {
                    SEND_MAX_PAGES
                };
                // a zero-copy send is limited to `ZC_MAX_SIZE`
                let max_size = if zc { ZC_MAX_SIZE as usize } else { SEND_MAX_SIZE };
                let buf = item
                    .ctx
                    .with_write_dst(|dst| SendData::take(dst, max_pages, max_size));
                let Some(mut buf) = buf else {
                    return;
                };
                let len = buf.len();
                let num = match &buf {
                    SendData::Page(_) => 1,
                    SendData::Pages(buf) => buf.pages().count(),
                };
                let use_zc = zc
                    && (ZC_SIZE as usize..=ZC_MAX_SIZE as usize).contains(&len)
                    && (num == 1 || !item.flags.contains(Flags::NO_ZC_MSG));
                let zc_report = use_zc && self.zc_report;
                let zc_flags = if zc_report { IORING_SEND_ZC_REPORT_USAGE } else { 0 };

                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: Snd({id}) size:{len} pages:{num} zc:{use_zc}",
                    item.ctx.tag(),
                );

                // SAFETY: a single page is heap backed, gathered pages are
                // stored in the boxed `SendBuf`, the data does not move until
                // the operation completes
                let entry = if num == 1 {
                    let ptr = match &buf {
                        SendData::Page(page) => unsafe { page.as_ptr() },
                        SendData::Pages(buf) => buf
                            .pages()
                            .next()
                            .map(|page| unsafe { page.as_ptr() })
                            .unwrap_or_default(),
                    };
                    let len = len as u32;
                    if use_zc {
                        opcode::SendZc::new(item.fd(), ptr, len)
                            .zc_flags(zc_flags)
                            .build()
                    } else {
                        opcode::Send::new(item.fd(), ptr, len).build()
                    }
                } else {
                    let SendData::Pages(buf) = &mut buf else {
                        unreachable!()
                    };
                    let SendBuf { pages, iov, msg } = &mut **buf;
                    for (iov, page) in iov.iter_mut().zip(pages.iter().flatten()) {
                        iov.iov_base = unsafe { page.as_ptr() }.cast_mut().cast();
                        iov.iov_len = page.len();
                    }
                    msg.msg_iov = iov.as_mut_ptr();
                    msg.msg_iovlen = num as _;
                    let msg = &raw const buf.msg;
                    if use_zc {
                        opcode::SendMsgZc::new(item.fd(), msg)
                            .ioprio(zc_flags)
                            .build()
                    } else {
                        opcode::SendMsg::new(item.fd(), msg).build()
                    }
                };

                let op_id = self.ops.insert(Some(buf.into_op(
                    id,
                    SendState {
                        result: None,
                        zc: use_zc,
                        zc_report,
                        resent: false,
                    },
                ))) as u32;
                item.wr_op = NonZeroU32::new(op_id);
                api.submit(op_id, entry);
            } else if item.flags.contains(Flags::WR_CANCELING) {
                item.flags.insert(Flags::WR_REISSUE);
            }
        }
    }

    fn add_operation(&mut self, op: Operation) -> u32 {
        self.ops.insert(Some(op)) as u32
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
        // Dropping while `StreamOps` handling event
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        log::trace!("{}: Close ({:?})", item.tag(), item.fd());

        item.flags.insert(Flags::CLOSING);

        // `Close` only removes the descriptor from the file table, in-flight
        // operations keep the socket open, so no FIN or RST is sent until
        // they complete. Operations are canceled by id, which does not
        // depend on the descriptor still being present in the table.
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
            .and_then(crate::helpers::shutdown_result)
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
