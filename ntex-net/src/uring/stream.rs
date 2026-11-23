use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, fmt, io, mem, num::NonZeroU32, ptr, rc::Rc, sync::OnceLock, task::Poll};

use ntex_bytes::{BufMut, BytePage, BytePages, Bytes, BytesMut, info::PageKind};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_io_uring::{cqueue, opcode, types::Fd, types::Fixed};
use ntex_rt::Arbiter;
use slab::Slab;
use socket2::Socket;

use super::{
    bufring::BufRing,
    reactor::{Handler, Reactor, ReactorApi},
};
use crate::helpers::Queue;

#[derive(Clone)]
pub(crate) struct StreamOps(Rc<StreamOpsInner>);

pub(crate) struct StreamCtl {
    id: StreamId,
    inner: Rc<StreamOpsInner>,
}

pub(crate) struct WeakStreamCtl {
    id: StreamId,
    inner: Rc<StreamOpsInner>,
}

enum IdType {
    Stream(StreamId),
    Weak(StreamId),
}

/// Slot of a stream and the generation of the stream registered in it.
///
/// Completions of a closed stream can arrive after its slot is reused, the
/// generation keeps them from reaching the new stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct StreamId {
    idx: u32,
    generation: u32,
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.idx, self.generation)
    }
}

/// Registered streams, lookups by a stale `StreamId` find nothing.
struct Streams {
    items: Slab<StreamItem>,
    generation: u32,
}

impl Streams {
    fn new() -> Self {
        Streams {
            items: Slab::new(),
            generation: 0,
        }
    }

    fn insert(&mut self, mut item: StreamItem) -> StreamId {
        let generation = self.generation;
        self.generation = generation.wrapping_add(1);
        item.generation = generation;
        let idx = u32::try_from(self.items.insert(item)).expect("Too many streams");
        StreamId { idx, generation }
    }

    fn get(&self, id: StreamId) -> Option<&StreamItem> {
        self.items
            .get(id.idx as usize)
            .filter(|item| item.generation == id.generation)
    }

    fn get_mut(&mut self, id: StreamId) -> Option<&mut StreamItem> {
        self.items
            .get_mut(id.idx as usize)
            .filter(|item| item.generation == id.generation)
    }

    fn remove(&mut self, id: StreamId) -> StreamItem {
        assert!(self.get(id).is_some(), "Stream {id} is gone");
        self.items.remove(id.idx as usize)
    }

    fn drain(&mut self) -> slab::Drain<'_, StreamItem> {
        self.items.drain()
    }
}

impl std::ops::Index<StreamId> for Streams {
    type Output = StreamItem;

    fn index(&self, id: StreamId) -> &StreamItem {
        self.get(id)
            .unwrap_or_else(|| panic!("Stream {id} is gone"))
    }
}

impl std::ops::IndexMut<StreamId> for Streams {
    fn index_mut(&mut self, id: StreamId) -> &mut StreamItem {
        self.get_mut(id)
            .unwrap_or_else(|| panic!("Stream {id} is gone"))
    }
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
        /// Input is streaming, receive into the connection's read buffer
        /// instead of a provided buffer
        const RD_DIRECT    = 0b1000_0000_0000;
        /// A receive returned eof or an error, the receive queue is empty for good
        const RD_EOF       = 0b1_0000_0000_0000;
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
/// Default size of the registered file table of a ring
const FIXED_FILES: u32 = 16 * 1024;
/// Kernel limit of the registered file table, `IORING_MAX_FIXED_FILES`
const FIXED_FILES_MAX: u64 = 1 << 20;
const FIXED_FILES_VAR: &str = "NTEX_URING_FIXED_FILES";

/// Expands `$op` with `$fd` set to the stream's slot in the registered file
/// table, or to its descriptor if it has no slot.
macro_rules! with_fd {
    ($item:expr, $fd:ident => $op:expr) => {
        match $item.slot {
            Some(slot) => {
                let $fd = Fixed(slot);
                $op
            }
            None => {
                let $fd = $item.fd();
                $op
            }
        }
    };
}

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    /// Slot in the registered file table, see `FixedFiles`
    slot: Option<u32>,
    /// Generation of the slot, see `StreamId`
    generation: u32,
    flags: Flags,
    rd_op: Option<NonZeroU32>,
    wr_op: Option<NonZeroU32>,
    ctx: IoContext,
}

#[derive(Debug)]
enum Operation {
    Recv {
        id: StreamId,
        buf: BytesMut,
    },
    /// Receive into a buffer selected from the provided buffer ring
    RecvRing {
        id: StreamId,
    },
    /// Send of gathered pages, or of an inline page
    Send {
        id: StreamId,
        buf: Box<SendBuf>,
        state: SendState,
    },
    /// Send of a single heap backed page, no allocation required
    SendOne {
        id: StreamId,
        page: BytePage,
        state: SendState,
    },
    /// Close of the stream's slot in the file table, or of its descriptor if
    /// it has no slot
    Close {
        id: StreamId,
    },
    /// Close of the descriptor of a stream with a slot
    CloseFd,
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
    fn into_send(self) -> (StreamId, SendData, SendState) {
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

    fn into_op(self, id: StreamId, state: SendState) -> Operation {
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
    default_flags: Flags,
}

struct StreamOpsStorage {
    ops: Slab<Option<Operation>>,
    streams: Streams,
    /// Provided buffers for receives that wait for input
    bufs: Option<BufRing>,
    /// Registered file table of the ring
    files: FixedFiles,
    /// `IORING_SEND_ZC_REPORT_USAGE` is supported, cleared on first `EINVAL`
    zc_report: bool,
}

/// Slots of the ring's registered file table, one per stream.
///
/// Operations on a registered socket skip the descriptor lookup and the file
/// reference count update the kernel does for every operation on a plain
/// descriptor. Streams beyond the table size use their descriptor.
///
/// A slot holds its own reference to the socket, the stream closes both the
/// slot and the descriptor. The slot is reused only after its `Close`
/// completed, a pending close would otherwise remove the next stream's socket.
#[derive(Debug)]
struct FixedFiles {
    /// Number of slots, zero if the table is not available
    cap: u32,
    /// Slots below `next` were handed out
    next: u32,
    free: Vec<u32>,
}

impl FixedFiles {
    fn new(api: &ReactorApi) -> Self {
        let mut cap = fixed_files_cap();
        if cap != 0
            && let Err(e) = api.register_files_sparse(cap)
        {
            // kernel older than 5.19, or the table does not fit
            log::debug!("Cannot register io-uring file table with {cap} slots: {e:?}");
            cap = 0;
        }
        FixedFiles {
            cap,
            next: 0,
            free: Vec::new(),
        }
    }

    /// Registers `fd` in a free slot, `None` if the table is full or not available.
    fn register(&mut self, api: &ReactorApi, fd: RawFd) -> Option<u32> {
        if self.cap == 0 {
            return None;
        }
        let slot = if let Some(slot) = self.free.pop() {
            slot
        } else if self.next < self.cap {
            self.next += 1;
            self.next - 1
        } else {
            return None;
        };

        match api.register_files_update(slot, &[fd]) {
            Ok(1) => Some(slot),
            res => {
                log::warn!("Cannot register socket in io-uring file table, disable: {res:?}");
                self.free.push(slot);
                self.cap = 0;
                None
            }
        }
    }

    /// Frees `slot`, its `Close` completed.
    fn release(&mut self, slot: u32) {
        self.free.push(slot);
    }

    /// Clears every slot, a socket still registered closes with its descriptor.
    fn clear(&mut self, api: &ReactorApi) {
        if self.next != 0 {
            if let Err(e) = api.register_files_update(0, &vec![-1; self.next as usize]) {
                log::error!("Cannot clear io-uring file table: {e:?}");
            }
            self.next = 0;
            self.free.clear();
        }
    }
}

/// Size of the registered file table of a ring, from `NTEX_URING_FIXED_FILES`.
fn fixed_files_cap() -> u32 {
    static CAP: OnceLock<u32> = OnceLock::new();

    *CAP.get_or_init(|| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `lim` is a valid `rlimit` for `getrlimit` to fill
        let nofile = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } == 0 {
            lim.rlim_cur
        } else {
            1024
        };
        parse_cap(std::env::var(FIXED_FILES_VAR).ok().as_deref(), nofile)
    })
}

/// Table size for the `NTEX_URING_FIXED_FILES` value `var`.
///
/// The kernel rejects a table larger than the open file limit `nofile`.
/// `0` disables the table.
fn parse_cap(var: Option<&str>, nofile: u64) -> u32 {
    let limit = nofile.min(FIXED_FILES_MAX) as u32;
    match var.map(str::trim).filter(|v| !v.is_empty()) {
        None => FIXED_FILES.min(limit),
        Some(v) => match v.parse::<u32>() {
            Ok(n) if n > limit => {
                log::info!("{FIXED_FILES_VAR}={n} exceeds the open file limit, using {limit}");
                limit
            }
            Ok(n) => n,
            Err(_) => {
                log::warn!("Invalid {FIXED_FILES_VAR} value {v:?}, using the default");
                FIXED_FILES.min(limit)
            }
        },
    }
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

                let mut ops = Slab::new();
                ops.insert(Some(Operation::Nop));
                let bufs = BufRing::new(&api);
                let files = FixedFiles::new(&api);

                let ops = Rc::new(StreamOpsInner {
                    api,
                    default_flags,
                    delayed_feed: Queue::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        ops,
                        streams: Streams::new(),
                        bufs,
                        files,
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
        let flags = if zc {
            self.0.default_flags
        } else {
            self.0.default_flags | Flags::NO_ZC
        };

        let id = self.0.with(|st| {
            let slot = st.files.register(&self.0.api, io.as_raw_fd());
            st.streams.insert(StreamItem {
                io,
                ctx,
                slot,
                flags,
                generation: 0,
                rd_op: None,
                wr_op: None,
            })
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

impl Handler for StreamOpsHandler {
    fn canceled(&mut self, user_data: usize) {
        self.inner
            .with(|st| match st.ops.remove(user_data).unwrap() {
                Operation::Recv { id, buf } => st.recv_canceled(id, Some(buf), &self.inner.api),
                // the kernel returns a selected buffer to the ring by itself
                Operation::RecvRing { id } => st.recv_canceled(id, None, &self.inner.api),
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
                Operation::Nop | Operation::Close { .. } | Operation::CloseFd => {}
            });
    }

    #[allow(clippy::too_many_lines)]
    fn completed(&mut self, user_data: usize, flags: u32, res: io::Result<usize>) {
        self.inner.with(|st| {
            match st.ops[user_data].take().unwrap() {
                Operation::Recv { id, buf } => {
                    st.recv_done(id, buf, flags, res, &self.inner.api);
                }
                Operation::RecvRing { id } => {
                    st.recv_ring_done(id, flags, res, &self.inner.api);
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
                Operation::Close { id } => {
                    // the kernel removed the socket from the slot
                    if let Some(slot) = st.streams[id].slot.take() {
                        st.files.release(slot);
                    }
                    if st.streams[id].flags.contains(Flags::DROPPED_SEC) {
                        let item = st.streams.remove(id);
                        #[cfg(feature = "trace")]
                        log::trace!("{}: Close({id})", item.ctx.tag());
                        mem::forget(item.io);
                    } else {
                        st.streams[id].flags.insert(Flags::DROPPED_PRI);
                    }
                }
                Operation::CloseFd | Operation::Nop => {}
            }
            let _ = st.ops.remove(user_data);
        });
    }

    fn tick(&mut self) {
        self.inner.check_delayed_feed();
    }

    fn in_flight(&self) -> usize {
        // slot 0 holds a placeholder that is never submitted
        self.inner.with(|st| st.ops.len() - 1)
    }

    fn abandon(&mut self) {
        // Operations and provided buffers live in `StreamOpsInner`, the
        // leaked handler keeps it alive, and the ring with it. Sockets are
        // still closed, `cleanup` clears the file table and closes the
        // descriptors: an issued operation holds its own file reference, so
        // the socket stays valid until it completes, while skipping the close
        // would leak the socket for the life of the process. This relies
        // on stream operations not using `IOSQE_ASYNC`, drain or links, which
        // resolve the file after `io_uring_enter` returns.
        self.cleanup();
    }

    fn cleanup(&mut self) {
        // Reactor has flushed pending submissions and canceled all in-flight
        // operations. Release every socket here, otherwise stored `IoContext`s
        // keep `StreamOpsInner` alive through the io handle and nothing closes.
        self.inner.try_with(|v| {
            // a registered socket stays open while its slot holds it
            v.files.clear(&self.inner.api);
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
        });
        self.inner.delayed_feed.clear();
    }
}

fn is_would_block(res: &io::Result<usize>) -> bool {
    matches!(res, Err(e) if e.kind() == io::ErrorKind::WouldBlock
        || e.raw_os_error() == Some(libc::EINPROGRESS))
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
    fn recv(&mut self, id: StreamId, poll_first: bool, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
            if item.rd_op.is_none() {
                // an idle connection does not hold a read buffer, the ring
                // provides one once input arrives. Input that completes a
                // partial frame is received into its buffer without a copy
                if let Some(ring) = &self.bufs
                    && !item.flags.contains(Flags::RD_DIRECT)
                    && item.ctx.read_buf_size() == 0
                {
                    #[cfg(feature = "trace")]
                    log::trace!("{}: RcvRing({id})", item.ctx.tag());

                    let op_id = self.ops.insert(Some(Operation::RecvRing { id })) as u32;
                    item.rd_op = NonZeroU32::new(op_id);

                    let (bgid, buf_len) = (ring.bgid(), ring.buf_len());
                    // SAFETY: the kernel selects a buffer from the registered
                    // ring, the descriptor is closed by a later `Close` entry
                    unsafe {
                        api.submit(op_id, move |entry| {
                            let op = with_fd!(item, fd => opcode::Recv::new(entry, fd, ptr::null_mut(), buf_len))
                                .buffer_select(bgid);
                            if poll_first {
                                op.ioprio(IORING_RECVSEND_POLL_FIRST);
                            }
                        });
                    }
                } else {
                    #[cfg(feature = "trace")]
                    log::trace!("{}: Rcv({id})", item.ctx.tag());

                    let mut buf = item.ctx.take_read_buf();
                    let s = buf.chunk_mut();
                    let buf_ptr = s.as_mut_ptr();
                    let buf_len = s.len() as u32;
                    let op_id = self.ops.insert(Some(Operation::Recv { id, buf })) as u32;
                    item.rd_op = NonZeroU32::new(op_id);

                    // SAFETY: the stored operation owns the heap buffer until
                    // the receive completes
                    unsafe {
                        api.submit(op_id, move |entry| {
                            let op = with_fd!(item, fd => opcode::Recv::new(entry, fd, buf_ptr, buf_len));
                            if poll_first {
                                op.ioprio(IORING_RECVSEND_POLL_FIRST);
                            }
                        });
                    }
                }
            } else if item.flags.contains(Flags::RD_CANCELING) {
                item.flags.insert(Flags::RD_REISSUE);
            }
        }
    }

    /// A receive was canceled, `buf` is the read buffer it took.
    ///
    /// A ring receive takes no read buffer. Reading was paused or the stream
    /// is closing, so the receive is reissued only for a resume that raced
    /// the cancel, it could not submit while the canceled receive was pending.
    /// Later resumes submit it themselves.
    fn recv_canceled(&mut self, id: StreamId, buf: Option<BytesMut>, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id) {
            #[cfg(feature = "trace")]
            log::trace!("{}: Recv canceled {:?}", item.tag(), item.fd());
            item.rd_op.take();
            item.flags.remove(Flags::RD_CANCELING);

            if let Some(buf) = buf {
                let _ = item.ctx.release_read_buf(buf, Poll::Pending);
            }
            if item.flags.contains(Flags::RD_REISSUE) {
                item.flags.remove(Flags::RD_REISSUE);
                self.recv(id, false, api);
            }
        }
    }

    fn recv_done(
        &mut self,
        id: StreamId,
        mut buf: BytesMut,
        flags: u32,
        res: io::Result<usize>,
        api: &ReactorApi,
    ) {
        if let Some(item) = self.streams.get_mut(id) {
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: RcvDone({id}) {res:?} non-empty:{}",
                item.ctx.tag(),
                cqueue::sock_nonempty(flags)
            );

            // reset op reference, a pending cancel lost the race
            // with this completion and has nothing to cancel anymore
            let _ = item.rd_op.take();
            item.flags.remove(Flags::RD_CANCELING | Flags::RD_REISSUE);

            if is_would_block(&res) {
                log::error!(
                    "{}: Received WouldBlock {:?}, id: {:?}",
                    item.tag(),
                    res,
                    item.ctx.id()
                );
                self.recv_more(id, buf, true, api);
            } else {
                if let Ok(size) = res
                    && size > 0
                {
                    // SAFETY: kernel tells us how many bytes it read
                    unsafe { buf.advance_mut(size) };
                }
                self.recv_complete(id, buf, flags, res, api);
            }
        }
    }

    fn recv_ring_done(
        &mut self,
        id: StreamId,
        flags: u32,
        res: io::Result<usize>,
        api: &ReactorApi,
    ) {
        let bid = cqueue::buffer_select(flags);
        if matches!(res, Err(ref e) if e.raw_os_error() == Some(libc::ECANCELED)) {
            // canceled after a buffer was selected
            if let (Some(bid), Some(ring)) = (bid, &mut self.bufs) {
                ring.recycle(bid);
            }
            self.recv_canceled(id, None, api);
            return;
        }
        let Some(item) = self.streams.get_mut(id) else {
            if let (Some(bid), Some(ring)) = (bid, &mut self.bufs) {
                ring.recycle(bid);
            }
            return;
        };

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: RcvRingDone({id}) {res:?} bid:{bid:?} non-empty:{}",
            item.ctx.tag(),
            cqueue::sock_nonempty(flags)
        );

        let _ = item.rd_op.take();
        item.flags.remove(Flags::RD_CANCELING | Flags::RD_REISSUE);

        if let (Some(bid), Some(ring)) = (bid, &mut self.bufs) {
            if let Ok(size) = res
                && size > 0
            {
                // the received page becomes the read buffer, it is charged
                // to the connection's tracker once it leaves the ring
                #[cfg_attr(not(feature = "tracker"), allow(unused_mut))]
                let mut buf = ring.take(bid, size);
                #[cfg(feature = "tracker")]
                if let Some(t) = item.ctx.buf_tracker() {
                    buf.set_tracker(t);
                }
                self.recv_complete(id, buf, flags, res, api);
                return;
            }
            ring.recycle(bid);
        }

        match res {
            Err(ref e) if e.raw_os_error() == Some(libc::ENOBUFS) => {
                // all ring buffers are in use, receive into a buffer of its own
                item.flags.insert(Flags::RD_DIRECT);
                self.recv(id, false, api);
            }
            ref res if is_would_block(res) => self.recv(id, true, api),
            res => {
                item.flags.insert(Flags::RD_EOF);
                let res = match res {
                    Ok(size) if size > 0 => Err(io::Error::other("Received data without a buffer")),
                    res => res,
                };
                if item.ctx.release_read_buf(BytesMut::new(), Poll::Ready(res)) == IoTaskStatus::Io
                {
                    self.recv(id, api.is_new(), api);
                }
            }
        }
    }

    /// Completes a receive into `buf`.
    fn recv_complete(
        &mut self,
        id: StreamId,
        buf: BytesMut,
        flags: u32,
        res: io::Result<usize>,
        api: &ReactorApi,
    ) {
        let item = &mut self.streams[id];
        let nonempty = cqueue::sock_nonempty(flags);
        // a socket error is reported only once queued data was read, and no
        // more data arrives after it. `ECANCELED` is routed to `canceled()`
        if matches!(res, Ok(0) | Err(_)) {
            item.flags.insert(Flags::RD_EOF);
        }

        // handle IORING_CQE_F_SOCK_NONEMPTY flag, more input is queued, keep
        // reading into the same buffer until it is full. The buffer is not
        // grown, so read backpressure applies once it is released
        if nonempty && buf.remaining_mut() > 0 && matches!(res, Ok(size) if size > 0) {
            self.recv_more(id, buf, false, api);
        } else {
            // input that fills the buffer is streaming, the next receive goes
            // to a buffer sized by the read history instead of a ring page
            item.flags
                .set(Flags::RD_DIRECT, nonempty || buf.remaining_mut() == 0);
            if item.ctx.release_read_buf(buf, Poll::Ready(res)) == IoTaskStatus::Io {
                self.recv(id, api.is_new(), api);
            }
        }
    }

    fn recv_more(&mut self, id: StreamId, mut buf: BytesMut, resize: bool, api: &ReactorApi) {
        if let Some(item) = self.streams.get_mut(id)
            && !item.flags.contains(Flags::CLOSING)
        {
            if resize {
                buf.reserve_more();
            }

            let slice = buf.chunk_mut();
            let buf_ptr = slice.as_mut_ptr();
            let buf_len = slice.len() as u32;
            let op_id = self.ops.insert(Some(Operation::Recv { id, buf })) as u32;
            item.rd_op = NonZeroU32::new(op_id);

            // SAFETY: the stored operation owns the heap buffer until the
            // receive completes
            unsafe {
                api.submit(op_id, move |entry| {
                    with_fd!(item, fd => opcode::Recv::new(entry, fd, buf_ptr, buf_len));
                });
            }
        }
    }

    fn send(&mut self, id: StreamId, api: &ReactorApi) {
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
                let Some(buf) = buf else {
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

                // Pointers are derived from the stored operation: the heap-backed
                // pages and boxed `SendBuf` stay in place until completion.
                let (ptr, msg) = match &mut self.ops[op_id as usize] {
                    Some(Operation::SendOne { page, .. }) => {
                        // SAFETY: This page is heap-backed; its storage is retained and unchanged through the final completion.
                        (unsafe { page.as_ptr() }, ptr::null())
                    }
                    Some(Operation::Send { buf, .. }) if num == 1 => {
                        let ptr = buf
                            .pages()
                            .next()
                            .map(|page| {
                                // SAFETY: The boxed send buffer keeps this page in place and unchanged through the final completion.
                                unsafe { page.as_ptr() }
                            })
                            .unwrap_or_default();
                        (ptr, ptr::null())
                    }
                    Some(Operation::Send { buf, .. }) => {
                        let SendBuf { pages, iov, msg } = &mut **buf;
                        for (iov, page) in iov.iter_mut().zip(pages.iter().flatten()) {
                            // SAFETY: The boxed send buffer keeps each page in place and unchanged through the final completion.
                            iov.iov_base = unsafe { page.as_ptr() }.cast_mut().cast();
                            iov.iov_len = page.len();
                        }
                        msg.msg_iov = iov.as_mut_ptr();
                        msg.msg_iovlen = num as _;
                        (ptr::null(), &raw const buf.msg)
                    }
                    _ => unreachable!(),
                };

                // SAFETY: see above, the pointers stay valid until the send
                // and its zero-copy notification complete
                unsafe {
                    api.submit(op_id, move |entry| match (msg.is_null(), use_zc) {
                        (true, true) => {
                            with_fd!(item, fd => opcode::SendZc::new(entry, fd, ptr, len as u32))
                                .zc_flags(zc_flags);
                        }
                        (true, false) => {
                            with_fd!(item, fd => opcode::Send::new(entry, fd, ptr, len as u32));
                        }
                        (false, true) => {
                            with_fd!(item, fd => opcode::SendMsgZc::new(entry, fd, msg))
                                .ioprio(zc_flags);
                        }
                        (false, false) => {
                            with_fd!(item, fd => opcode::SendMsg::new(entry, fd, msg));
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

    fn pause_read(&mut self, id: StreamId, api: &ReactorApi) {
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        if let Some(rd_op) = item.rd_op {
            if item.flags.contains(Flags::RD_CANCELING) {
                // paused again before the cancel completed
                item.flags.remove(Flags::RD_REISSUE);
            } else {
                item.flags.insert(Flags::RD_CANCELING);
                api.cancel(rd_op.get());
                log::trace!("{}: Recv to pause ({:?})", item.tag(), item.fd());
            }
        }
    }

    fn drop_stream(&mut self, id: StreamId, api: &ReactorApi) {
        // Dropping while `StreamOps` handling event
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        log::trace!("{}: Close ({:?})", item.tag(), item.fd());

        item.flags.insert(Flags::CLOSING);

        // `Close` only removes the descriptor or the slot from its table,
        // in-flight operations keep the socket open, so no FIN or RST is sent
        // until they complete. Operations are canceled by id, which does not
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

        let fd = item.fd();
        let slot = item.slot;
        let op_id = self.add_operation(Operation::Close { id });
        if let Some(slot) = slot {
            // the slot and the descriptor each hold a reference to the socket
            let fd_op_id = self.add_operation(Operation::CloseFd);
            // SAFETY: the entries hold no pointers, the closes own the slot
            // and the descriptor
            unsafe {
                api.submit(op_id, move |entry| {
                    opcode::Close::new(entry, Fixed(slot));
                });
                api.submit(fd_op_id, move |entry| {
                    opcode::Close::new(entry, fd);
                });
            }
        } else {
            // SAFETY: the entry holds no pointers, the close owns the descriptor
            unsafe {
                api.submit(op_id, move |entry| {
                    opcode::Close::new(entry, fd);
                });
            }
        }
    }

    fn drop_weak_stream(&mut self, id: StreamId) {
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

impl Drop for StreamOpsInner {
    fn drop(&mut self) {
        if let Some(st) = self.storage.get_mut()
            && let Some(ring) = &mut st.bufs
            && let Err(e) = ring.unregister(&self.api)
        {
            log::error!("Cannot unregister provided buffer ring: {e:?}");
        }
    }
}

impl StreamOpsInner {
    /// Runs `f` with the storage.
    ///
    /// # Panics
    ///
    /// Panics if the storage is already in use, a re-entrant call.
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StreamOpsStorage) -> R,
    {
        self.try_with(f)
            .expect("io-uring stream storage is in use, re-entrant call")
    }

    /// Runs `f` with the storage, returns `None` if it is already in use.
    ///
    /// Aborts the process if `f` panics. The storage owns buffers and sockets
    /// of in-flight operations, dropping it while unwinding would release
    /// memory the kernel still writes to.
    fn try_with<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut StreamOpsStorage) -> R,
    {
        let mut storage = self.storage.take()?;
        // declared after `storage`, so it is dropped first while unwinding
        let guard = ntex_rt::AbortOnUnwind("io-uring stream operation panicked");
        let result = f(&mut storage);
        guard.disarm();
        self.storage.set(Some(storage));
        Some(result)
    }

    fn check_delayed_feed(&self) {
        self.try_with(|storage| {
            while let Some(id) = self.delayed_feed.pop() {
                match id {
                    IdType::Stream(id) => storage.drop_stream(id, &self.api),
                    IdType::Weak(id) => storage.drop_weak_stream(id),
                }
            }
        });
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
    pub(crate) fn shutdown(&self, terminate: bool) -> io::Result<()> {
        if terminate {
            // The connection was force-closed, so it is released without the
            // graceful close: the receive queue is not drained and no
            // `shutdown(2)` is called. The socket is aborted instead, so that
            // the peer sees an RST and cannot mistake a truncated stream for a
            // complete one. Dropping this handle submits the `Close` that
            // releases the descriptor.
            self.inner.with(|storage| {
                storage.pause_read(self.id, &self.inner.api);
                let fd = storage.streams[self.id].fd().0;
                // SAFETY: the stream entry owns the fd until this handle is dropped
                unsafe { crate::helpers::with_sock_ref(fd, crate::helpers::abort_socket) };
            });
            return Ok(());
        }

        self.inner.with(|storage| {
            storage.pause_read(self.id, &self.inner.api);
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: Shutdown ({:?})",
                storage.streams[self.id].ctx.tag(),
                self.id
            );
            let fd = storage.streams[self.id].fd().0;
            let item = &storage.streams[self.id];
            let drain = item.rd_op.is_none() && !item.flags.contains(Flags::RD_EOF);
            // `shutdown(2)` is called directly: io-uring always runs its
            // `Shutdown` opcode on an io-wq worker thread, which costs a thread
            // wakeup and preempts the reactor for every closed connection.
            // Write readiness reports `Close` only after every send completed,
            // so no queued send can be overtaken.
            // SAFETY: the stream entry owns the fd until this handle is dropped
            unsafe {
                crate::helpers::with_sock_ref(fd, |sock| {
                    if drain {
                        // `pause_read()` above only submits a cancel, so a recv
                        // can still be in flight; draining now would race it.
                        // Nothing is lost by skipping, that recv empties the
                        // queue itself. After eof or a reset the queue is empty
                        // and the peer cannot send more, nothing is left to drain.
                        crate::helpers::drain_socket(sock);
                    }
                    crate::helpers::shutdown_result(sock.shutdown(std::net::Shutdown::Both))
                })
            }
        })
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
        let dropped = self
            .inner
            .try_with(|storage| storage.drop_stream(self.id, &self.inner.api));
        if dropped.is_none() {
            self.inner.delayed_feed.push(IdType::Stream(self.id));
        }
    }
}

impl WeakStreamCtl {
    /// Runs `f` with the socket.
    ///
    /// Returns `None` if the stream is gone, or if called while the storage
    /// is in use, from a filter that processes received data for example.
    pub(crate) fn with_io<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner
            .try_with(|storage| storage.streams.get(self.id).map(|item| f(&item.io)))
            .flatten()
    }

    /// Submits a send for buffered output, without the write task.
    ///
    /// The write status is reported once the transport took the output or
    /// has nothing to take, so the write task stays paused; the send
    /// completion continues writing. Does nothing if the stream is closing
    /// or gone, or if called while the storage is in use, the write task
    /// sends the output then.
    pub(crate) fn write(&self, ctx: &IoContext) {
        let submitted = self.inner.try_with(|storage| {
            storage.send(self.id, &self.inner.api);
            storage
                .streams
                .get(self.id)
                .is_some_and(|item| !item.flags.contains(Flags::CLOSING))
        });
        if submitted == Some(true) {
            ctx.update_write_status(Ok(0));
        }
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        let dropped = self
            .inner
            .try_with(|storage| storage.drop_weak_stream(self.id));
        if dropped.is_none() {
            self.inner.delayed_feed.push(IdType::Weak(self.id));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use ntex_io::{Handle, Io, IoStream};
    use ntex_service::cfg::SharedCfg;

    use super::*;

    struct TestStream {
        socket: Socket,
        ops: StreamOps,
        ctl: Rc<Cell<Option<StreamCtl>>>,
    }

    struct TestHandle {
        _ctl: WeakStreamCtl,
    }

    impl Handle for TestHandle {}

    impl IoStream for TestStream {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            let (ctl, weak) = self.ops.register(self.socket, ctx, false);
            self.ctl.set(Some(ctl));
            Box::new(TestHandle { _ctl: weak })
        }
    }

    /// Registers one end of a socket pair, returns the peer as well.
    fn register(ops: &StreamOps) -> (Io, StreamCtl, UnixStream) {
        let (socket, peer) = UnixStream::pair().unwrap();
        let ctl = Rc::new(Cell::new(None));
        let io = Io::new(
            TestStream {
                socket: Socket::from(socket),
                ops: ops.clone(),
                ctl: ctl.clone(),
            },
            SharedCfg::default(),
        );
        (io, ctl.take().unwrap(), peer)
    }

    /// Returns the id of the first stored operation that matches `f`.
    fn find_op(ops: &StreamOps, f: impl Fn(&Operation) -> bool) -> Option<usize> {
        ops.0.with(|st| {
            st.ops
                .iter()
                .find_map(|(op_id, op)| op.as_ref().filter(|op| f(op)).map(|_| op_id))
        })
    }

    /// STATEMENT: The file table size defaults to 16384 slots, follows
    /// `NTEX_URING_FIXED_FILES`, and never exceeds the open file limit or the kernel
    /// limit.
    ///
    /// HOW:
    /// - Parse no value, a blank and an invalid value, check the default.
    /// - Parse no value with an open file limit of 1024, check the default is clamped.
    /// - Parse `0`, a value within the limit and values above the open file limit or
    ///   the kernel limit, check each result.
    #[test]
    fn fixed_files_cap_from_env() {
        assert_eq!(parse_cap(None, 1 << 20), 16384);
        assert_eq!(parse_cap(Some(" "), 1 << 20), 16384);
        assert_eq!(parse_cap(Some("many"), 1 << 20), 16384);
        assert_eq!(parse_cap(None, 1024), 1024);
        assert_eq!(parse_cap(Some("0"), 1024), 0);
        assert_eq!(parse_cap(Some(" 100 "), 1024), 100);
        assert_eq!(parse_cap(Some("4096"), 1024), 1024);
        assert_eq!(parse_cap(Some("4000000"), u64::MAX), 1 << 20);
    }

    /// STATEMENT: A stream's slot in the registered file table is reused only after
    /// the `Close` of the slot completed, not after the close of its descriptor.
    ///
    /// HOW:
    /// - Register a stream and check it got a slot.
    /// - Drop its control and check a `Close` and a `CloseFd` were queued.
    /// - Complete `CloseFd`, check the slot is still taken and a new stream gets
    ///   another slot.
    /// - Complete `Close`, check the slot is free and the next stream gets it.
    #[ntex::test]
    async fn slot_reused_after_its_close() {
        let Ok(reactor) = Reactor::new(64) else {
            return;
        };
        let ops = StreamOps::get(&reactor);
        if ops.0.with(|st| st.files.cap) == 0 {
            // kernel without sparse file tables
            return;
        }
        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };

        let (io, ctl, _peer) = register(&ops);
        let id = ctl.id;
        let (fd, slot) = ops
            .0
            .with(|st| (st.streams[id].fd().0, st.streams[id].slot));
        let slot = slot.unwrap();

        drop(ctl);
        let close_op = find_op(&ops, |op| matches!(op, Operation::Close { .. })).unwrap();
        let close_fd_op = find_op(&ops, |op| matches!(op, Operation::CloseFd)).unwrap();

        handler.completed(close_fd_op, 0, Ok(0));
        assert!(
            ops.0
                .with(|st| st.streams[id].slot == Some(slot) && st.files.free.is_empty())
        );
        let (_io2, ctl2, _peer2) = register(&ops);
        let other = ops.0.with(|st| st.streams[ctl2.id].slot);
        assert!(other.is_some_and(|other| other != slot));

        handler.completed(close_op, 0, Ok(0));
        assert!(
            ops.0
                .with(|st| st.streams[id].slot.is_none() && st.files.free == [slot])
        );
        drop(io);
        handler.tick();
        // SAFETY: `Close` was never submitted, so `fd` is still open and nothing else owns it.
        unsafe { libc::close(fd) };

        let (_io3, ctl3, _peer3) = register(&ops);
        assert_eq!(ops.0.with(|st| st.streams[ctl3.id].slot), Some(slot));
    }

    /// STATEMENT: A stream registered while the file table is full uses its descriptor
    /// and is closed by a single `Close`.
    ///
    /// HOW:
    /// - Register a stream, then shrink the table to the slots handed out.
    /// - Register another stream and check it has no slot.
    /// - Drop its control, check a `Close` was queued for it and no `CloseFd`.
    #[ntex::test]
    async fn full_table_falls_back_to_descriptor() {
        let Ok(reactor) = Reactor::new(64) else {
            return;
        };
        let ops = StreamOps::get(&reactor);
        if ops.0.with(|st| st.files.cap) == 0 {
            // kernel without sparse file tables
            return;
        }

        let (_io, _ctl, _peer) = register(&ops);
        ops.0.with(|st| st.files.cap = st.files.next);
        let (_io2, ctl2, _peer2) = register(&ops);
        let id = ctl2.id;
        assert!(ops.0.with(|st| st.streams[id].slot.is_none()));

        drop(ctl2);
        assert!(
            find_op(
                &ops,
                |op| matches!(op, Operation::Close { id: c } if *c == id)
            )
            .is_some()
        );
        assert!(find_op(&ops, |op| matches!(op, Operation::CloseFd)).is_none());
    }

    /// STATEMENT: Cleanup at runtime stop closes a socket of a live stream even though
    /// its slot in the file table holds a reference to it.
    ///
    /// HOW:
    /// - Register a stream and check it got a slot.
    /// - Run the handler cleanup while the stream is alive.
    /// - Read the peer with a 5 s timeout and expect eof.
    #[ntex::test]
    async fn cleanup_closes_registered_socket() {
        let Ok(reactor) = Reactor::new(64) else {
            return;
        };
        let ops = StreamOps::get(&reactor);
        if ops.0.with(|st| st.files.cap) == 0 {
            // kernel without sparse file tables
            return;
        }
        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };

        let (_io, ctl, mut peer) = register(&ops);
        assert!(ops.0.with(|st| st.streams[ctl.id].slot.is_some()));
        handler.cleanup();

        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(std::io::Read::read(&mut peer, &mut buf).unwrap(), 0);
    }

    /// STATEMENT: A completion that arrives after its stream was removed does not reach
    /// a new stream registered in the same slot.
    ///
    /// HOW:
    /// - Register a stream and store a `Recv` operation for it, as if the receive was
    ///   still in flight.
    /// - Close the stream, complete its `Close` and drop the `Io`, so the slot is freed.
    /// - Register a new stream and check it reuses the slot with another generation.
    /// - Complete the stale receive with data and check the operation is released, the
    ///   new stream received nothing and no receive was issued for it.
    #[ntex::test]
    async fn stale_completion_skips_reused_slot() {
        let Ok(reactor) = Reactor::new(64) else {
            return;
        };
        let ops = StreamOps::get(&reactor);
        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };

        let (io, ctl, _peer) = register(&ops);
        let old = ctl.id;
        let fd = ops.0.with(|st| st.streams[old].fd().0);
        let mut buf = BytesMut::with_capacity(64);
        buf.extend_from_slice(b"stale");
        buf.clear();
        let recv_op = ops
            .0
            .with(|st| st.add_operation(Operation::Recv { id: old, buf }));

        drop(ctl);
        let close_op = ops.0.with(|st| {
            st.ops
                .iter()
                .find_map(|(op_id, op)| {
                    matches!(op, Some(Operation::Close { .. })).then_some(op_id)
                })
                .unwrap()
        });
        handler.completed(close_op, 0, Ok(0));
        drop(io);
        handler.tick();
        assert!(ops.0.with(|st| st.streams.get(old).is_none()));
        // `Close` was never submitted, the kernel did not close the descriptor
        // SAFETY: `Close` was never submitted, so `fd` is still open and nothing else owns it.
        unsafe { libc::close(fd) };

        let (io, ctl, _peer) = register(&ops);
        assert_eq!(ctl.id.idx, old.idx);
        assert_ne!(ctl.id.generation, old.generation);

        handler.completed(recv_op as usize, 0, Ok(5));
        assert!(ops.0.with(|st| !st.ops.contains(recv_op as usize)));
        assert_eq!(io.read_dst_size(), 0);
        assert!(ops.0.with(|st| st.streams[ctl.id].rd_op.is_none()));
    }

    /// STATEMENT: A graceful shutdown after a receive returned eof or a connection
    /// reset does not read the socket, the receive queue is empty for good then.
    ///
    /// HOW:
    /// - For eof and for `ECONNRESET`: register a stream and complete its receive
    ///   with that result.
    /// - Queue 4 bytes from the peer, which a real socket cannot see after eof or a
    ///   reset, so a drain would be observable.
    /// - Shut the stream down and check with `FIONREAD` the 4 bytes are still queued.
    #[ntex::test]
    async fn shutdown_after_eof_skips_drain() {
        let Ok(reactor) = Reactor::new(64) else {
            return;
        };
        let ops = StreamOps::get(&reactor);
        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };

        for res in [Ok(0), Err(libc::ECONNRESET)] {
            let (_io, ctl, mut peer) = register(&ops);
            ctl.resume_read();
            let (fd, rd_op) = ops
                .0
                .with(|st| (st.streams[ctl.id].fd().0, st.streams[ctl.id].rd_op.unwrap()));
            let res = res.map_err(io::Error::from_raw_os_error);
            handler.completed(rd_op.get() as usize, 0, res);
            assert!(ops.0.with(|st| st.streams[ctl.id].rd_op.is_none()));

            std::io::Write::write_all(&mut peer, b"data").unwrap();
            ctl.shutdown(false).unwrap();

            let mut queued: libc::c_int = 0;
            // SAFETY: `fd` is open, `queued` is a valid `c_int` for `FIONREAD`
            let res = unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut queued) };
            assert_eq!(res, 0);
            assert_eq!(queued, 4);
        }
    }

    /// STATEMENT: `SendBuf` debug output shows the number of gathered pages and their
    /// total length.
    ///
    /// HOW:
    /// - Format an empty buffer, then one holding two pages of 5 and 3 bytes.
    #[test]
    fn send_buf_debug() {
        let mut buf = SendBuf::new();
        assert_eq!(format!("{buf:?}"), "SendBuf { pages: 0, len: 0 }");

        buf.pages[0] = Some(BytePage::from(Bytes::from_static(b"hello")));
        buf.pages[1] = Some(BytePage::from(Bytes::from_static(b"abc")));
        assert_eq!(format!("{buf:?}"), "SendBuf { pages: 2, len: 8 }");
    }
}
