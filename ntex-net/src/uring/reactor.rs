use std::cell::{Cell, UnsafeCell};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsRawFd, RawFd};
use std::panic::{self, AssertUnwindSafe};
use std::time::{Duration, Instant};
use std::{collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_io_uring::cqueue::{self, Entry as CEntry, more};
use ntex_io_uring::opcode::{self, AsyncCancel};
use ntex_io_uring::squeue::{self, Entry as SEntry, PushError, SubmissionQueue};
use ntex_io_uring::types::{CancelBuilder, SubmitArgs, Timespec};
use ntex_io_uring::{EnterFlags, IoUring, Probe, Submitter};
use ntex_rt::{DriverType, Notify, PollResult, Runtime};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{
    TcpStream, UnixStream,
    notifier::{self, Notifier, NotifierRead},
    stream::StreamOps,
};
use crate::channel::Receiver;

pub trait Handler {
    /// Operation is completed.
    fn completed(&mut self, id: usize, flags: u32, result: io::Result<usize>);

    /// Operation is canceled.
    fn canceled(&mut self, id: usize);

    /// The driver's turn has completed.
    fn tick(&mut self);

    /// Number of submitted operations without their final completion.
    ///
    /// The driver waits for them on shutdown, see [`cleanup`](Self::cleanup).
    fn in_flight(&self) -> usize {
        0
    }

    /// Clean up the handle before dropping the driver.
    ///
    /// Called on shutdown, after the operations counted by
    /// [`in_flight`](Self::in_flight) completed.
    fn cleanup(&mut self);

    /// Operations are still in flight when the driver stops.
    ///
    /// Called instead of [`cleanup`](Self::cleanup). The handler is leaked
    /// afterwards, so memory referenced by the operations stays valid. It must
    /// not release anything the kernel may still use. Descriptors may be
    /// closed, the shutdown flushed every submission and the kernel holds its
    /// own file reference for each issued operation.
    fn abandon(&mut self) {}
}

/// Api for io-uring reactor.
pub struct ReactorApi {
    batch: u64,
    inner: Rc<ReactorInner>,
}

impl ReactorApi {
    #[inline]
    #[deprecated(
        since = "4.13.1",
        note = "the driver requires Linux 6.1, always `true`"
    )]
    /// Check if the kernel is 6.0 or greater, always `true`.
    pub fn is_new(&self) -> bool {
        true
    }

    fn submit_inner<F>(&self, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        self.inner.push(f);
    }

    #[inline]
    /// Submit request to the driver.
    ///
    /// `f` initializes a zeroed submission entry in place with an opcode.
    ///
    /// # Safety
    ///
    /// The kernel uses the entry after this call returns. Every buffer,
    /// address and file descriptor the entry refers to must stay valid, and
    /// must not move, until the handler receives the completion for `id`.
    /// Canceling the request does not end this, the completion still arrives.
    ///
    /// `f` runs while the entry is borrowed from the submission queue, it
    /// must not submit or cancel requests on this reactor. Debug builds panic
    /// if it does.
    pub unsafe fn submit<F>(&self, id: u32, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        self.submit_inner(|en| {
            f(en);
            en.set_user_data(u64::from(id) | self.batch);
        });
    }

    #[inline]
    /// Submit request to the driver without delivering its completion.
    ///
    /// For requests whose result is not needed, the `Close` of a descriptor
    /// that nothing else uses for example. A successful request posts no
    /// completion, a failure is ignored.
    ///
    /// # Safety
    ///
    /// The entry must not refer to memory, nothing tells when the kernel is
    /// done with the request. A file descriptor or slot it refers to must
    /// stay valid until the request is issued, with the next submit of the
    /// reactor. See [`submit`](Self::submit) for the restrictions of `f`.
    pub unsafe fn submit_detached<F>(&self, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        self.submit_inner(|en| {
            f(en);
            en.set_flags(squeue::Flags::SKIP_SUCCESS);
            en.set_user_data(Reactor::CANCEL);
        });
    }

    #[inline]
    /// Attempt to cancel an already issued request.
    pub fn cancel(&self, id: u32) {
        self.submit_inner(|en| {
            AsyncCancel::new(en, u64::from(id) | self.batch).user_data(Reactor::CANCEL);
        });
    }

    /// Get whether a specific io-uring opcode is supported.
    pub fn is_supported(&self, opcode: u8) -> bool {
        self.inner.probe.is_supported(opcode)
    }

    /// Id of the handler, unique within the reactor.
    pub fn id(&self) -> u16 {
        (self.batch >> Reactor::BATCH) as u16
    }

    /// Registers a provided buffer ring.
    ///
    /// # Safety
    ///
    /// The ring at `ring_addr` with `entries` entries must stay valid until
    /// it is unregistered with
    /// [`unregister_buf_ring`](Self::unregister_buf_ring).
    pub unsafe fn register_buf_ring(
        &self,
        ring_addr: u64,
        entries: u16,
        bgid: u16,
    ) -> io::Result<()> {
        // SAFETY: The caller guarantees the ring remains valid until it is unregistered.
        unsafe {
            self.inner
                .ring
                .submitter()
                .register_buf_ring_with_flags(ring_addr, entries, bgid, 0)
        }
    }

    /// Unregisters a provided buffer ring.
    pub fn unregister_buf_ring(&self, bgid: u16) -> io::Result<()> {
        self.inner.ring.submitter().unregister_buf_ring(bgid)
    }

    /// Marks a submitted request that usually completes during its
    /// submission, a send to a socket with buffer space for example.
    ///
    /// A turn that handled few completions submits it before it ends, and
    /// handles the completion within the turn rather than in another one.
    pub(crate) fn submitted_inline(&self) {
        self.inner.inline.set(true);
    }

    #[cfg(test)]
    pub(crate) fn take_inline(&self) -> bool {
        self.inner.inline.take()
    }

    /// Registers an empty file table with `nr` slots.
    ///
    /// The ring has one table, it is released with the ring.
    pub fn register_files_sparse(&self, nr: u32) -> io::Result<()> {
        self.inner.ring.submitter().register_files_sparse(nr)
    }

    /// Replaces slots of the file table starting at `offset`, `-1` clears a slot.
    ///
    /// Issued operations keep the file they resolved, a replaced or cleared
    /// file is released once they complete.
    pub fn register_files_update(&self, offset: u32, fds: &[RawFd]) -> io::Result<usize> {
        self.inner
            .ring
            .submitter()
            .register_files_update(offset, fds)
    }
}

/// io-uring reactor.
pub struct Reactor {
    fd: RawFd,
    hid: Cell<u64>,
    notifier: Notifier,
    #[allow(clippy::box_collection)]
    handlers: Cell<Option<Box<Vec<HandlerItem>>>>,
    inner: Rc<ReactorInner>,
}

struct HandlerItem {
    hnd: Box<dyn Handler>,
    modified: bool,
}

impl HandlerItem {
    fn tick(&mut self) {
        if self.modified {
            self.modified = false;
            self.hnd.tick();
        }
    }
}

/// Handlers taken out of the reactor, put back when dropped.
///
/// Handlers own the memory of their in-flight operations. Put back while
/// unwinding, they are released by the reactor shutdown once the kernel is
/// done with it, instead of being dropped with the panic.
#[allow(clippy::box_collection)]
struct TakenHandlers<'a> {
    slot: &'a Cell<Option<Box<Vec<HandlerItem>>>>,
    handlers: Option<Box<Vec<HandlerItem>>>,
}

impl<'a> TakenHandlers<'a> {
    #[allow(clippy::box_collection)]
    fn new(slot: &'a Cell<Option<Box<Vec<HandlerItem>>>>) -> Self {
        let handlers = Some(slot.take().unwrap_or_default());
        Self { slot, handlers }
    }
}

impl Deref for TakenHandlers<'_> {
    type Target = Vec<HandlerItem>;

    fn deref(&self) -> &Self::Target {
        self.handlers.as_ref().unwrap()
    }
}

impl DerefMut for TakenHandlers<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.handlers.as_mut().unwrap()
    }
}

impl Drop for TakenHandlers<'_> {
    fn drop(&mut self) {
        self.slot.set(self.handlers.take());
    }
}

/// Opcodes the driver issues, all available since 6.1
const REQUIRED_OPCODES: [(u8, &str); 11] = [
    (opcode::Accept::CODE, "Accept"),
    (opcode::AsyncCancel::CODE, "AsyncCancel"),
    (opcode::Close::CODE, "Close"),
    (opcode::Connect::CODE, "Connect"),
    (opcode::PollAdd::CODE, "PollAdd"),
    (opcode::Read::CODE, "Read"),
    (opcode::Recv::CODE, "Recv"),
    (opcode::Send::CODE, "Send"),
    (opcode::SendMsg::CODE, "SendMsg"),
    (opcode::SendMsgZc::CODE, "SendMsgZc"),
    (opcode::SendZc::CODE, "SendZc"),
];

/// Name of the first required opcode the kernel does not support.
fn missing_opcode(probe: &Probe) -> Option<&'static str> {
    REQUIRED_OPCODES
        .iter()
        .find(|(code, _)| !probe.is_supported(*code))
        .map(|(_, name)| *name)
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("io-uring driver requires Linux 6.1+ ({what})"),
    )
}

struct ReactorInner {
    probe: Probe,
    ring: IoUring<SEntry, CEntry>,
    changes: UnsafeCell<VecDeque<mem::MaybeUninit<SEntry>>>,
    notify_read: NotifierRead,
    /// A request submitted in the current turn usually completes during the
    /// submission, see `ReactorApi::submitted_inline`
    inline: Cell<bool>,
    #[cfg(debug_assertions)]
    pushing: Cell<bool>,
}

#[cfg(debug_assertions)]
struct PushGuard<'a>(&'a Cell<bool>);

#[cfg(debug_assertions)]
impl Drop for PushGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl ReactorInner {
    fn push<F>(&self, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        // `f` runs while the entry is borrowed from a queue, see `submit`
        #[cfg(debug_assertions)]
        let _guard = {
            assert!(
                !self.pushing.replace(true),
                "submit or cancel called from a `submit` callback"
            );
            PushGuard(&self.pushing)
        };

        // SAFETY: Submission is thread-local and non-reentrant; zeroing initializes every SQE field
        // before `f` uses it, and only the run loop publishes the queue tail.
        unsafe {
            let changes = &mut *self.changes.get();
            // the queue tail is published by the run loop submit
            let f = if changes.is_empty() {
                match self.ring.submission_unsynced().push(f) {
                    Ok(()) => return,
                    Err(PushError(f)) => f,
                }
            } else {
                f
            };
            changes.push_back(mem::MaybeUninit::uninit());
            let entry = changes.back_mut().unwrap();
            ptr::write_bytes(entry.as_mut_ptr(), 0, 1);
            f(entry.assume_init_mut());
        }
    }
}

impl Drop for ReactorInner {
    fn drop(&mut self) {
        self.notify_read.stop(&self.ring);
    }
}

impl Reactor {
    const NOTIFY: u64 = notifier::NOTIFY;
    /// User data of cancel and detached requests, their completions are ignored
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
    /// Completions handled before running tasks. Smaller chunks keep
    /// sockets hot in the cache between read completion and write, about
    /// 15% faster than handling the whole queue at once.
    const CQES_PER_POLL: usize = 16;

    /// Default submission queue size, the completion queue is twice as large.
    ///
    /// Submissions are flushed mid-turn and on queue overflow, a small ring
    /// keeps both queues in cache.
    // Unused when the polling, tokio or compio runtime is selected.
    #[allow(dead_code)]
    pub(crate) const DEFAULT_ENTRIES: u32 = 256;

    /// Create io-uring driver
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] on kernels older than 6.1,
    /// detected by the ring setup flags, features and opcodes the kernel
    /// supports.
    pub fn new(capacity: u32) -> io::Result<Self> {
        let build = |no_sqarray: bool| {
            let mut builder = IoUring::builder();
            builder
                .setup_coop_taskrun()
                .setup_single_issuer()
                .setup_defer_taskrun();
            if no_sqarray {
                // available since 6.6
                builder.setup_no_sqarray();
            }
            builder.build(capacity)
        };

        // Create ring
        let ring = if let Ok(ring) = build(true) {
            log::info!(
                "New io-uring driver with single-issuer, coop-taskrun, defer-taskrun, no-sqarray"
            );
            ring
        } else {
            match build(false) {
                Ok(ring) => {
                    log::info!(
                        "New io-uring driver with single-issuer, coop-taskrun, defer-taskrun"
                    );
                    ring
                }
                // unknown setup flags, `IORING_SETUP_DEFER_TASKRUN` is available since 6.1
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    return Err(unsupported("IORING_SETUP_DEFER_TASKRUN"));
                }
                Err(e) => return Err(e),
            }
        };

        // implied by the setup flags, checked for kernels with backported features
        let params = ring.params();
        if !params.is_feature_ext_arg() {
            return Err(unsupported("IORING_FEAT_EXT_ARG"));
        }
        if !params.is_feature_skip_cqe_on_success() {
            return Err(unsupported("IORING_FEAT_CQE_SKIP"));
        }

        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        if let Some(name) = missing_opcode(&probe) {
            return Err(unsupported(name));
        }

        // Avoid the per-call ring fd lookup in io_uring_enter. The
        // registration is per-thread, the reactor is created and driven on
        // the same thread.
        if let Err(e) = ring.submitter().register_ring_fd() {
            log::debug!("Cannot register io-uring ring fd: {e}");
        }

        let fd = ring.as_raw_fd();
        let inner = Rc::new(ReactorInner {
            ring,
            probe,
            changes: UnsafeCell::new(VecDeque::with_capacity(32)),
            notify_read: NotifierRead::new(),
            inline: Cell::new(false),
            #[cfg(debug_assertions)]
            pushing: Cell::new(false),
        });

        // Remote notifier
        let notifier = Notifier::new()?;

        Ok(Self {
            fd,
            inner,
            notifier,
            hid: Cell::new(0),
            handlers: Cell::new(Some(Box::new(Vec::new()))),
        })
    }

    /// Driver type
    pub const fn tp(&self) -> DriverType {
        DriverType::IoUring
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(ReactorApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = TakenHandlers::new(&self.handlers);
        handlers.push(HandlerItem {
            hnd: f(ReactorApi {
                batch: id << Self::BATCH,
                inner: self.inner.clone(),
            }),
            modified: false,
        });
        drop(handlers);
        self.hid.set(id + 1);
    }

    fn apply_changes(&self, sq: SubmissionQueue<'_, SEntry>) -> bool {
        // SAFETY: The thread-local overflow entries are initialized and disjoint from the SQ;
        // no submission callback runs while they are copied.
        unsafe {
            let changes = &mut *self.inner.changes.get();
            // safety: "changes" contains only initialized entries
            while let Some(entry) = changes.front() {
                let res = sq.push(|en| ptr::copy_nonoverlapping(entry.as_ptr(), en, 1));
                if res.is_err() {
                    break;
                }
                changes.pop_front();
            }

            let read = &self.inner.notify_read;
            if changes.is_empty() {
                read.arm(sq, self.notifier.as_raw_fd());
            }
            !changes.is_empty() || read.is_queued()
        }
    }

    /// Submit all pending changes to the kernel
    fn flush(&self, sq: SubmissionQueue<'_, SEntry>, submitter: &Submitter<'_>) {
        let mut retries = 0;
        loop {
            let more_changes = self.apply_changes(sq);

            if let Err(e) = submitter.submit() {
                match e.raw_os_error() {
                    Some(libc::ETIME | libc::EBUSY | libc::EAGAIN | libc::EINTR)
                        if retries < 16 =>
                    {
                        retries += 1;
                        continue;
                    }
                    _ => {
                        log::error!("Cannot submit pending operations: {e:?}");
                        break;
                    }
                }
            }
            if !more_changes {
                break;
            }
        }
    }

    /// Cancels in-flight operations and releases handlers.
    ///
    /// Handlers own the buffers, addresses and sockets of their operations.
    /// A handler is cleaned up only once the kernel is done with all of them,
    /// otherwise it is abandoned and leaked.
    fn shutdown(
        &self,
        sq: SubmissionQueue<'_, SEntry>,
        submitter: &Submitter<'_>,
        cq: &mut cqueue::CompletionQueue<'_, CEntry>,
    ) {
        let ring = &self.inner.ring;
        self.inner.notify_read.stop_arming();
        // publish the entries consumed in the last turn before entering the kernel
        cq.sync();
        // Operations queued during the last turn (`Close` of dropped
        // streams in particular) have not reached the kernel yet.
        self.flush(sq, submitter);

        let canceled = submitter.register_sync_cancel(None, CancelBuilder::any());
        self.inner.notify_read.canceled(ring, &canceled);
        if let Err(e) = canceled {
            log::error!("Cannot cancel in-flight operations: {e:?}");
        }

        let Some(handlers) = self.handlers.take() else {
            return;
        };

        // Completions are counted, not delivered. Handlers would issue new
        // operations, a resend of canceled output for example.
        let mut in_flight: Vec<_> = handlers.iter().map(|h| h.hnd.in_flight()).collect();
        let deadline = Instant::now() + Self::SHUTDOWN_TIMEOUT;
        loop {
            cq.sync();
            for entry in &mut *cq {
                match entry.user_data() {
                    Self::CANCEL => {}
                    Self::NOTIFY => self.inner.notify_read.completed(entry.result()),
                    // the final completion, a zero-copy send ends with its notification
                    user_data if !more(entry.flags()) => {
                        let batch = ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                        if let Some(n) = in_flight.get_mut(batch) {
                            *n = n.saturating_sub(1);
                        }
                    }
                    _ => {}
                }
            }
            cq.sync();

            if in_flight.iter().all(|n| *n == 0) {
                break;
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                break;
            }
            let ts = Timespec::from(timeout);
            if let Err(e) = submitter.submit_with_args(1, &SubmitArgs::new().timespec(&ts))
                && !matches!(
                    e.raw_os_error(),
                    Some(libc::ETIME | libc::EINTR | libc::EBUSY | libc::EAGAIN)
                )
            {
                log::error!("Cannot wait for in-flight operations: {e:?}");
                break;
            }
        }

        for (mut h, n) in handlers.into_iter().zip(in_flight) {
            if n == 0 {
                h.hnd.cleanup();
            } else {
                log::error!(
                    "{n} io-uring operations are in flight on shutdown, leaking their memory"
                );
                h.hnd.abandon();
                mem::forget(h);
            }
        }
    }

    /// Handle up to `CQES_PER_POLL` synced completions, forward changes to
    /// specific handler. Returns the number of handled completions, and
    /// `true` if synced completions remain.
    ///
    /// The caller must sync the queue before and after.
    fn poll_completions(&self, cq: &mut cqueue::CompletionQueue<'_, CEntry>) -> (usize, bool) {
        let mut handled = 0;
        let mut has_more = false;

        if !cqueue::CompletionQueue::<'_, _>::is_empty(cq) {
            handled = ExactSizeIterator::len(cq).min(Self::CQES_PER_POLL);
            let mut handlers = TakenHandlers::new(&self.handlers);
            for entry in (&mut *cq).take(Self::CQES_PER_POLL) {
                let user_data = entry.user_data();
                match user_data {
                    Self::CANCEL => {}
                    Self::NOTIFY => self.inner.notify_read.completed(entry.result()),
                    _ => {
                        let batch = ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                        let user_data = (user_data & Self::DATA_MASK) as usize;

                        let result = entry.result();
                        // A canceled zero-copy send still posts a notification,
                        // the operation must stay alive until it arrives. A
                        // canceled operation that selected a provided buffer
                        // completes, the handler must return the buffer.
                        if result == -libc::ECANCELED
                            && !more(entry.flags())
                            && cqueue::buffer_select(entry.flags()).is_none()
                        {
                            handlers[batch].modified = true;
                            handlers[batch].hnd.canceled(user_data);
                        } else {
                            // zero-copy notification result is a set of flags
                            let result = if result < 0 && !cqueue::notif(entry.flags()) {
                                Err(io::Error::from_raw_os_error(-result))
                            } else {
                                #[allow(clippy::cast_sign_loss)]
                                Ok(result as u32 as _)
                            };
                            handlers[batch].modified = true;
                            handlers[batch]
                                .hnd
                                .completed(user_data, entry.flags(), result);
                        }
                    }
                }
            }
            has_more = ExactSizeIterator::len(cq) != 0;

            for h in handlers.iter_mut() {
                h.tick();
            }
        }
        (handled, has_more)
    }
}

/// Submit the queue and post deferred completions without waiting.
///
/// With deferred task running only an enter with `GETEVENTS` runs task work,
/// a plain submit never posts completions that arrived since the last wait.
fn submit_getevents(
    sq: SubmissionQueue<'_, SEntry>,
    submitter: &Submitter<'_>,
) -> io::Result<usize> {
    sq.sync();
    // SAFETY: Valid arguments for an enter without extra arguments.
    let res = unsafe {
        submitter.enter::<libc::sigset_t>(sq.len() as u32, 0, EnterFlags::GETEVENTS.bits(), None)
    };
    // the kernel consumes entries during the enter, even if it fails
    sq.sync();
    res
}

#[cfg(test)]
impl Reactor {
    /// Submits the queued entries and passes the posted completions to the
    /// handlers, without a runtime and without arming the notifier.
    pub(crate) fn submit_and_dispatch(&self) {
        let ring = &self.inner.ring;
        // SAFETY: No other borrow of the change queue is live.
        assert!(unsafe { (*self.inner.changes.get()).is_empty() });
        // SAFETY: The caller drives the reactor, it is the only completion consumer.
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        submit_getevents(ring.submission(), &ring.submitter()).unwrap();
        cq.sync();
        while self.poll_completions(&mut cq).1 {
            cq.sync();
        }
        cq.sync();
    }
}

impl AsRawFd for Reactor {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

// Sockets are left in their mode, which saves the `fcntl` calls of
// `set_nonblocking()`. The io-uring socket operations do not depend on
// `O_NONBLOCK`, they never block the submitting thread and wait for readiness
// themselves. The direct syscalls on a socket (`recv` with `MSG_DONTWAIT`,
// `shutdown`, socket options) do not block either.
impl crate::Reactor for Reactor {
    fn tcp_connect(&self, addr: net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let addr = SockAddr::from(addr);
        let result = Socket::new(addr.domain(), Type::STREAM, Some(Protocol::TCP))
            .and_then(crate::helpers::prep_tcp_socket)
            .map(move |sock| (addr, sock));

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => super::connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        let result = SockAddr::unix(addr).and_then(|addr| {
            Socket::new(addr.domain(), Type::STREAM, None).map(move |sock| (addr, sock))
        });

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => super::connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
        Ok(Io::new(
            TcpStream(Socket::from(stream), StreamOps::get(self)),
            cfg,
        ))
    }

    #[cfg(unix)]
    fn from_unix_stream(&self, stream: OsUnixStream, cfg: SharedCfg) -> io::Result<Io> {
        Ok(Io::new(
            UnixStream(Socket::from(stream), StreamOps::get(self)),
            cfg,
        ))
    }

    fn tcp_listener(&self, sock: net::TcpListener) -> io::Result<crate::Listener> {
        Ok(super::accept::AcceptOps::get(self).listener(sock))
    }

    fn is_tcp_listener_supported(&self) -> bool {
        true
    }
}

impl ntex_rt::Driver for Reactor {
    /// Poll the driver and handle completed operations.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        let _notifier = self.inner.notify_read.guard(&self.inner.ring);
        let ring = &self.inner.ring;
        let sq = ring.submission();
        // SAFETY: This run loop is the sole completion consumer; callbacks never borrow the CQ.
        let mut cq = unsafe { ring.completion_shared() };
        let submitter = ring.submitter();
        // A panic of a task or a handler must not skip the shutdown, which
        // releases handlers only after the kernel is done with their memory.
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            loop {
                // tasks woken until the runtime is polled do not need to notify
                rt.awake();
                cq.sync();
                self.inner.inline.set(false);

                // Run tasks after each chunk of completions, so a task writes
                // to its socket while the socket is still hot in the cache.
                // All chunks belong to one runtime turn, and their submissions
                // share one `io_uring_enter` unless the submission queue fills.
                let mut events = 0;
                let mut reaped_inline = false;
                let more_cqes = loop {
                    let (handled, more) = self.poll_completions(&mut cq);
                    events += handled;
                    if !more {
                        if handled == 0 || reaped_inline {
                            break false;
                        }
                        // Requests that complete during their submission, sends
                        // in particular, are submitted before the turn ends and
                        // their completions handled in it. Otherwise the wait
                        // at the end of the turn returns at once, and the next
                        // turn handles only these completions. A busy turn
                        // leaves them to the next one, which handles them with
                        // new input.
                        rt.run_ready();
                        if !self.inner.inline.take() || events >= Self::CQES_PER_POLL {
                            break false;
                        }
                        reaped_inline = true;
                        cq.sync();
                        if submit_getevents(sq, &submitter).is_err() {
                            // the end of the turn retries and reports the error
                            break false;
                        }
                        self.apply_changes(sq);
                        cq.sync();
                        continue;
                    }
                    rt.run_ready();

                    if sq.len() >= 96 {
                        // flush the full queue without ending the turn; consumed
                        // entries are published first, the enter posts completions
                        // that arrived during the turn
                        cq.sync();
                        if submit_getevents(sq, &submitter).is_err() {
                            // the end of the turn retries and reports the error
                            break true;
                        }
                        self.apply_changes(sq);
                        // SAFETY: No other borrow of the change queue is live.
                        if unsafe { !(*self.inner.changes.get()).is_empty() } {
                            break true;
                        }
                    }
                };

                let (more_tasks, timeout) = match rt.poll() {
                    PollResult::Pending(timeout) => (false, timeout),
                    PollResult::PollAgain => (true, None),
                    PollResult::Ready => break Ok(()),
                };
                let more_changes = self.apply_changes(sq);

                // Publish the consumed entries before entering the kernel, it
                // would count them as pending completions and return from the
                // next wait immediately. Without deferred task running,
                // completions may arrive during the turn, waiting would return
                // immediately.
                cq.sync();

                // submit publishes the queue tail, and skips the syscall if
                // there is nothing to submit and no deferred completions to post.
                let result = if more_tasks {
                    // a task that keeps the runtime busy would starve completions
                    submit_getevents(sq, &submitter)
                } else if more_changes
                    || more_cqes
                    || !cqueue::CompletionQueue::<'_, _>::is_empty(&cq)
                {
                    submitter.submit()
                } else if let Some(timeout) = timeout {
                    // wake up for the next batch of runtime timers
                    let ts = Timespec::from(timeout);
                    submitter.submit_with_args(1, &SubmitArgs::new().timespec(&ts))
                } else {
                    submitter.submit_and_wait(1)
                };

                if let Err(e) = result {
                    match e.raw_os_error() {
                        Some(libc::ETIME | libc::EBUSY | libc::EAGAIN | libc::EINTR) => {
                            log::debug!("Ring submit interrupted, {e:?}");
                        }
                        _ => break Err(e),
                    }
                }
            }
        }));

        // unwinding out of the shutdown would drop handlers with operations in flight
        let guard = ntex_rt::AbortOnUnwind("io-uring reactor shutdown panicked");
        self.shutdown(sq, &submitter, &mut cq);
        guard.disarm();
        result.unwrap_or_else(|payload| panic::resume_unwind(payload))
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(self.notifier.handle())
    }
}

impl fmt::Debug for Reactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reactor")
            .field("fd", &self.fd)
            .field("hid", &self.hid)
            .field("notifier", &self.notifier)
            .finish()
    }
}

impl fmt::Debug for ReactorApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReactorApi")
            .field("batch", &self.batch)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::future::pending;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::task::{Poll, Waker};

    use ntex_io_uring::opcode::{Close, Nop, Read};
    use ntex_io_uring::types::Fd;
    use ntex_rt::syscall;

    use super::notifier::NotifierState;
    use super::*;

    /// STATEMENT: Queued changes that wrap around the deque storage are submitted in order.
    ///
    /// HOW:
    /// - Fill and drain the change deque so four `Nop` entries wrap its end.
    /// - Run `apply_changes()` and check the deque is empty and four entries are queued.
    /// - Submit them and check the completions carry user data `0..4` in order.
    #[test]
    fn apply_wrapped_changes() {
        let Ok(reactor) = Reactor::new(4) else {
            return;
        };
        let ring = &reactor.inner.ring;
        let sq = ring.submission();
        // submit the notifier
        assert!(!reactor.apply_changes(sq));
        ring.submitter().submit().unwrap();
        sq.sync();
        assert_eq!(sq.len(), 0);

        // queued entries wrap around the end of the deque storage; the borrow
        // ends before `apply_changes()` takes its own
        {
            // SAFETY: The test has the only access to the queue; the borrow ends before `apply_changes()`.
            let changes = unsafe { &mut *reactor.inner.changes.get() };
            let cap = changes.capacity();
            let nop = |user_data| {
                let mut entry = SEntry::default();
                Nop::new(&mut entry).user_data(user_data);
                mem::MaybeUninit::new(entry)
            };
            for _ in 0..cap - 2 {
                changes.push_back(nop(0));
            }
            for _ in 0..cap - 2 {
                changes.pop_front();
            }
            for i in 0..4 {
                changes.push_back(nop(i));
            }
            assert_eq!(changes.as_slices().0.len(), 2);
        }

        assert!(!reactor.apply_changes(sq));
        sq.sync();
        // SAFETY: No other borrow of the change queue is live.
        assert!(unsafe { &*reactor.inner.changes.get() }.is_empty());
        assert_eq!(sq.len(), 4);

        ring.submitter().submit_and_wait(4).unwrap();
        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        let ids: Vec<_> = cq.map(|e| e.user_data()).collect();
        assert_eq!(ids, [0, 1, 2, 3]);
    }

    /// STATEMENT: `poll_completions` handles at most `CQES_PER_POLL` synced completions
    /// and reports how many it handled and whether more remain.
    ///
    /// HOW:
    /// - Submit `CQES_PER_POLL + 4` `Nop` operations to a counting handler and wait for them.
    /// - Sync once, poll and check `CQES_PER_POLL` completions were handled and reported,
    ///   with `true` for the rest.
    /// - Poll again and check the other 4 were handled and reported, `false` returned and
    ///   the queue is empty.
    #[test]
    fn poll_completions_handles_one_chunk() {
        struct Count(Rc<Cell<usize>>);
        impl Handler for Count {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {
                self.0.set(self.0.get() + 1);
            }
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Ok(reactor) = Reactor::new(32) else {
            return;
        };
        let count = Rc::new(Cell::new(0));
        let mut api = None;
        reactor.register(|a| {
            api = Some(a);
            Box::new(Count(count.clone()))
        });
        let api = api.unwrap();
        let total = Reactor::CQES_PER_POLL + 4;
        for id in 0..u32::try_from(total).unwrap() {
            // SAFETY: `Nop` entries reference no memory or descriptors.
            unsafe {
                api.submit(id, |en| {
                    Nop::new(en);
                });
            }
        }
        reactor
            .inner
            .ring
            .submitter()
            .submit_and_wait(total)
            .unwrap();

        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { reactor.inner.ring.completion_shared() };
        cq.sync();
        assert_eq!(
            reactor.poll_completions(&mut cq),
            (Reactor::CQES_PER_POLL, true)
        );
        assert_eq!(count.get(), Reactor::CQES_PER_POLL);
        assert_eq!(reactor.poll_completions(&mut cq), (4, false));
        assert_eq!(count.get(), total);
        cq.sync();
        assert!(cq.is_empty());
    }

    /// STATEMENT: A submission queue filled by a chunk of completions is flushed
    /// without ending the runtime turn.
    ///
    /// HOW:
    /// - Complete two chunks of `Nop` operations on a ring with one chunk of submission entries.
    /// - Resubmit a `Nop` for each completion of the first chunk, filling the submission queue.
    /// - Record `poll_count()` from a task woken by completions and check the task saw
    ///   both chunks handled within one poll, and that all resubmitted operations complete.
    #[test]
    fn full_submission_queue_keeps_turn() {
        struct Resubmit {
            api: ReactorApi,
            count: Rc<Cell<usize>>,
            budget: usize,
            waker: Rc<Cell<Option<Waker>>>,
        }
        impl Handler for Resubmit {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {
                self.count.set(self.count.get() + 1);
                if self.budget > 0 {
                    self.budget -= 1;
                    // SAFETY: `Nop` entries reference no memory or descriptors.
                    unsafe {
                        self.api.submit(1000, |en| {
                            Nop::new(en);
                        });
                    }
                }
                if let Some(waker) = self.waker.take() {
                    waker.wake();
                }
            }
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let chunk = Reactor::CQES_PER_POLL;
        let Ok(reactor) = Reactor::new(u32::try_from(chunk).unwrap()) else {
            return;
        };
        let ring = &reactor.inner.ring;
        let count = Rc::new(Cell::new(0));
        let waker = Rc::new(Cell::new(None));
        reactor.register(|api| {
            for id in 0..u32::try_from(2 * chunk).unwrap() {
                if id == u32::try_from(chunk).unwrap() {
                    ring.submitter().submit().unwrap();
                }
                // SAFETY: `Nop` entries reference no memory or descriptors.
                unsafe {
                    api.submit(id, |en| {
                        Nop::new(en);
                    });
                }
            }
            Box::new(Resubmit {
                api,
                count: count.clone(),
                budget: chunk,
                waker: waker.clone(),
            })
        });
        ring.submitter().submit_and_wait(2 * chunk).unwrap();

        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let seen = RefCell::new(Vec::new());
        rt.block_on(
            std::future::poll_fn(|cx| {
                waker.set(Some(cx.waker().clone()));
                seen.borrow_mut().push((count.get(), rt.poll_count()));
                if count.get() == 3 * chunk {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
            &reactor,
        );

        let seen = seen.into_inner();
        let polls = |n| seen.iter().find(|(c, _)| *c == n).map(|(_, p)| *p);
        assert!(polls(chunk).is_some(), "{seen:?}");
        assert_eq!(polls(chunk), polls(2 * chunk), "{seen:?}");
    }

    /// STATEMENT: A request marked as completing during its submission is submitted
    /// before the turn ends, and its completion is handled in the same turn.
    ///
    /// HOW:
    /// - Complete a `Nop` whose handler submits another `Nop` and calls `submitted_inline()`.
    /// - Record the completion count and `poll_count()` from a task woken by completions.
    /// - Check both completions were seen with the same poll count, so one runtime turn
    ///   handled them.
    #[test]
    fn inline_completion_is_handled_in_turn() {
        struct Resubmit {
            api: ReactorApi,
            count: Rc<Cell<usize>>,
            waker: Rc<Cell<Option<Waker>>>,
        }
        impl Handler for Resubmit {
            fn completed(&mut self, id: usize, _: u32, _: io::Result<usize>) {
                self.count.set(self.count.get() + 1);
                if id == 0 {
                    // SAFETY: `Nop` entries reference no memory or descriptors.
                    unsafe {
                        self.api.submit(1, |en| {
                            Nop::new(en);
                        });
                    }
                    self.api.submitted_inline();
                }
                if let Some(waker) = self.waker.take() {
                    waker.wake();
                }
            }
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Ok(reactor) = Reactor::new(16) else {
            return;
        };
        let count = Rc::new(Cell::new(0));
        let waker = Rc::new(Cell::new(None));
        reactor.register(|api| {
            // SAFETY: `Nop` entries reference no memory or descriptors.
            unsafe {
                api.submit(0, |en| {
                    Nop::new(en);
                });
            }
            Box::new(Resubmit {
                api,
                count: count.clone(),
                waker: waker.clone(),
            })
        });
        reactor.inner.ring.submitter().submit_and_wait(1).unwrap();

        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let seen = RefCell::new(Vec::new());
        rt.block_on(
            std::future::poll_fn(|cx| {
                waker.set(Some(cx.waker().clone()));
                seen.borrow_mut().push((count.get(), rt.poll_count()));
                if count.get() == 2 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }),
            &reactor,
        );

        let seen = seen.into_inner();
        let polls = |n| seen.iter().find(|(c, _)| *c == n).map(|(_, p)| *p);
        assert!(polls(1).is_some(), "{seen:?}");
        assert_eq!(polls(1), polls(2), "{seen:?}");
    }

    /// STATEMENT: A detached request posts no completion when it succeeds, a failed one
    /// posts a completion with the user data the reactor ignores.
    ///
    /// HOW:
    /// - Submit a detached `Nop` and a detached `Close` of an invalid descriptor.
    /// - Submit and wait for one completion; expect only the `EBADF` of the close, with
    ///   `Reactor::CANCEL` as user data.
    #[test]
    fn detached_request_posts_only_failures() {
        struct Noop;
        impl Handler for Noop {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {}
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Ok(reactor) = Reactor::new(8) else {
            return;
        };
        reactor.register(|api| {
            // SAFETY: The entries reference no memory, `-1` is not a descriptor.
            unsafe {
                api.submit_detached(|en| {
                    Nop::new(en);
                });
                api.submit_detached(|en| {
                    Close::new(en, Fd(-1));
                });
            }
            Box::new(Noop)
        });
        let ring = &reactor.inner.ring;
        ring.submitter().submit_and_wait(1).unwrap();
        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        let entries: Vec<_> = (&mut cq).map(|e| (e.user_data(), e.result())).collect();
        assert_eq!(entries, [(Reactor::CANCEL, -libc::EBADF)]);
    }

    /// STATEMENT: A kernel that lacks an opcode the driver issues is rejected.
    ///
    /// HOW:
    /// - Check an empty `Probe`, a kernel that supports no opcode, names the
    ///   first required opcode as missing.
    #[test]
    fn kernel_without_required_opcode_is_rejected() {
        assert_eq!(missing_opcode(&Probe::new()), Some("Accept"));
    }

    /// STATEMENT: In debug builds, a `submit` or `cancel` from a `submit` callback panics
    /// before it touches the entry being built, on the ring and on the overflow path.
    ///
    /// HOW:
    /// - With a free submission queue, submit from the callback of a `submit` and check
    ///   it panics with the nested submit message and nothing is queued.
    /// - Fill the submission queue so entries go to the overflow deque, cancel from the
    ///   callback and check the same panic, leaving only the zeroed outer entry.
    /// - Submit once more and check the flag was reset by the unwind.
    #[cfg(debug_assertions)]
    #[test]
    fn nested_submit_panics() {
        struct Noop;
        impl Handler for Noop {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {}
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Ok(reactor) = Reactor::new(4) else {
            return;
        };
        let mut api = None;
        reactor.register(|a| {
            api = Some(a);
            Box::new(Noop)
        });
        let api = api.unwrap();
        let message = |err: Box<dyn std::any::Any + Send>| {
            err.downcast_ref::<&str>().copied().unwrap_or_default()
        };
        let changes = || {
            // SAFETY: Called between submissions, when no other borrow of the queue is live.
            unsafe { &*reactor.inner.changes.get() }.len()
        };
        let ring = &reactor.inner.ring;

        let err = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `Nop` entries reference no memory; the nested call panics before submitting.
            unsafe {
                api.submit(1, |en| {
                    Nop::new(en);
                    api.submit(2, |en| {
                        Nop::new(en);
                    });
                });
            }
        }))
        .unwrap_err();
        assert_eq!(
            message(err),
            "submit or cancel called from a `submit` callback"
        );
        assert!(ring.submission().is_empty());
        assert_eq!(changes(), 0);

        for _ in 0..4 {
            reactor.inner.push(|en| {
                Nop::new(en);
            });
        }
        assert!(ring.submission().is_full());
        let err = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `Nop` entries reference no memory; the nested cancel panics.
            unsafe {
                api.submit(1, |en| {
                    Nop::new(en);
                    api.cancel(2);
                });
            }
        }))
        .unwrap_err();
        assert_eq!(
            message(err),
            "submit or cancel called from a `submit` callback"
        );
        assert_eq!(changes(), 1);

        // SAFETY: `Nop` entries reference no memory or descriptors.
        unsafe {
            api.submit(3, |en| {
                Nop::new(en);
            });
        }
        assert_eq!(changes(), 2);
    }

    #[derive(Default)]
    struct Events {
        cleanup: Cell<bool>,
        abandon: Cell<bool>,
    }

    struct Tracked {
        events: Rc<Events>,
        in_flight: usize,
    }

    impl Handler for Tracked {
        fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {}
        fn canceled(&mut self, _: usize) {}
        fn tick(&mut self) {}
        fn in_flight(&self) -> usize {
            self.in_flight
        }
        fn abandon(&mut self) {
            self.events.abandon.set(true);
        }
        fn cleanup(&mut self) {
            self.events.cleanup.set(true);
        }
    }

    /// STATEMENT: On shutdown, in-flight operations are canceled and their handler is
    /// cleaned up and released only after the kernel completed them.
    ///
    /// HOW:
    /// - Register a handler that submits a read from an empty pipe into a buffer it owns.
    /// - Run the reactor until the runtime finishes.
    /// - Check `cleanup` was called, `abandon` was not, and the handler was dropped.
    /// - Write to the pipe and check the byte is still readable, so the read was gone.
    #[test]
    fn shutdown_cancels_in_flight() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        let mut fds = [0; 2];
        syscall!(libc::pipe(fds.as_mut_ptr())).unwrap();
        // SAFETY: `pipe` returned two fresh descriptors that nothing else owns.
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

        let events = Rc::new(Events::default());
        let mut buf = Box::new([0u8; 8]);
        let buf_ptr = buf.as_mut_ptr();
        let fd = rd.as_raw_fd();
        reactor.register(|api| {
            // SAFETY: `buf` and `rd` outlive the reactor run, which waits until the read is canceled.
            unsafe {
                api.submit(1, |en| {
                    Read::new(en, Fd(fd), buf_ptr, 8);
                });
            }
            Box::new(Tracked {
                events: events.clone(),
                in_flight: 1,
            })
        });
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        rt.block_on(async {}, &reactor);

        assert!(events.cleanup.get());
        assert!(!events.abandon.get());
        assert_eq!(Rc::strong_count(&events), 1);
        drop(buf);

        syscall!(libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK)).unwrap();
        syscall!(libc::write(wr.as_raw_fd(), b"x".as_ptr().cast(), 1)).unwrap();
        let mut out = [0u8; 8];
        let n = syscall!(libc::read(fd, out.as_mut_ptr().cast(), 8)).unwrap();
        assert_eq!(&out[..n.cast_unsigned()], b"x");
    }

    /// STATEMENT: A handler whose operations do not complete within the shutdown
    /// timeout is abandoned and leaked instead of released.
    ///
    /// HOW:
    /// - Register a handler that reports one in-flight operation it never submitted.
    /// - Run the reactor until the runtime finishes.
    /// - Check `abandon` was called, `cleanup` was not, and the handler was not dropped.
    #[test]
    fn shutdown_leaks_stuck_handler() {
        let Ok(reactor) = Reactor::new(8) else {
            return;
        };
        let events = Rc::new(Events::default());
        reactor.register(|_| {
            Box::new(Tracked {
                events: events.clone(),
                in_flight: 1,
            })
        });
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let start = Instant::now();
        rt.block_on(async {}, &reactor);

        assert!(start.elapsed() >= Reactor::SHUTDOWN_TIMEOUT);
        assert!(events.abandon.get());
        assert!(!events.cleanup.get());
        drop(rt);
        drop(reactor);
        assert_eq!(Rc::strong_count(&events), 2);
    }

    /// Panics on completions.
    struct PanicOnComplete(Tracked);

    impl Handler for PanicOnComplete {
        fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {
            panic!("handler panicked");
        }
        fn canceled(&mut self, _: usize) {}
        fn tick(&mut self) {}
        fn in_flight(&self) -> usize {
            self.0.in_flight()
        }
        fn abandon(&mut self) {
            self.0.abandon();
        }
        fn cleanup(&mut self) {
            self.0.cleanup();
        }
    }

    /// Registers a handler that reads from an empty pipe into `buf`, after
    /// a no-op if `nop` is set, and returns the pipe ends.
    fn register_pipe_read(
        reactor: &Reactor,
        buf: &mut [u8; 8],
        nop: bool,
        hnd: Box<dyn Handler>,
    ) -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        syscall!(libc::pipe(fds.as_mut_ptr())).unwrap();
        // SAFETY: `pipe` returned two fresh descriptors that nothing else owns.
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let (fd, buf_ptr) = (rd.as_raw_fd(), buf.as_mut_ptr());
        reactor.register(|api| {
            // SAFETY: the caller keeps `buf` and the pipe alive until the read is canceled.
            unsafe {
                api.submit(1, |en| {
                    Read::new(en, Fd(fd), buf_ptr, 8);
                });
                if nop {
                    api.submit(2, |en| {
                        Nop::new(en);
                    });
                }
            }
            hnd
        });
        (rd, wr)
    }

    /// Checks a byte written to the pipe is still readable, so no read is pending.
    fn assert_pipe_unread(rd: &OwnedFd, wr: &OwnedFd) {
        let fd = rd.as_raw_fd();
        syscall!(libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK)).unwrap();
        syscall!(libc::write(wr.as_raw_fd(), b"x".as_ptr().cast(), 1)).unwrap();
        let mut out = [0u8; 8];
        let n = syscall!(libc::read(fd, out.as_mut_ptr().cast(), 8)).unwrap();
        assert_eq!(&out[..n.cast_unsigned()], b"x");
    }

    /// STATEMENT: A task panic unwinds out of the reactor only after the shutdown
    /// canceled in-flight operations and released their handler.
    ///
    /// HOW:
    /// - Register a handler that submits a read from an empty pipe into a buffer it owns.
    /// - Run a future that panics, and check the panic reaches the caller.
    /// - Check `cleanup` was called, `abandon` was not, and the handler was dropped.
    /// - Write to the pipe and check the byte is still readable, so the read was gone.
    #[test]
    fn task_panic_runs_shutdown() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        let events = Rc::new(Events::default());
        let mut buf = Box::new([0u8; 8]);
        let hnd = Box::new(Tracked {
            events: events.clone(),
            in_flight: 1,
        });
        let (rd, wr) = register_pipe_read(&reactor, &mut buf, false, hnd);

        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let res = catch_unwind(AssertUnwindSafe(|| {
            rt.block_on(async { panic!("task panicked") }, &reactor);
        }));
        assert!(res.is_err());

        assert!(events.cleanup.get());
        assert!(!events.abandon.get());
        assert_eq!(Rc::strong_count(&events), 1);
        drop(buf);
        assert_pipe_unread(&rd, &wr);
    }

    /// STATEMENT: A handler panic during completion handling keeps the handlers in
    /// the reactor, so the shutdown cancels their in-flight operations before
    /// releasing them.
    ///
    /// HOW:
    /// - Register a handler that panics on completion, and submit a no-op and a read
    ///   from an empty pipe into a buffer it owns.
    /// - Run a pending future, the no-op completion panics, and check the panic
    ///   reaches the caller.
    /// - Check `cleanup` was called, `abandon` was not, and the handler was dropped.
    /// - Write to the pipe and check the byte is still readable, so the read was gone.
    #[test]
    fn handler_panic_runs_shutdown() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        let events = Rc::new(Events::default());
        let mut buf = Box::new([0u8; 8]);
        let hnd = Box::new(PanicOnComplete(Tracked {
            events: events.clone(),
            in_flight: 1,
        }));
        let (rd, wr) = register_pipe_read(&reactor, &mut buf, true, hnd);

        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let res = catch_unwind(AssertUnwindSafe(|| {
            rt.block_on(pending::<()>(), &reactor);
        }));
        assert!(res.is_err());

        assert!(events.cleanup.get());
        assert!(!events.abandon.get());
        assert_eq!(Rc::strong_count(&events), 1);
        drop(buf);
        assert_pipe_unread(&rd, &wr);
    }

    fn sync_cancel_reactor() -> Option<Reactor> {
        let reactor = Reactor::new(8).ok()?;
        let err = reactor
            .inner
            .ring
            .submitter()
            .register_sync_cancel(None, CancelBuilder::user_data(Reactor::NOTIFY))
            .unwrap_err();
        if err.raw_os_error() != Some(libc::ENOENT) {
            eprintln!("Skipping notifier cancellation test: {err}");
            return None;
        }
        Some(reactor)
    }

    fn assert_notifier_canceled(reactor: &Reactor) {
        // Run deferred cancellation completions without submitting other SQEs.
        // Only an enter with `GETEVENTS` runs deferred task work, and the
        // canceled read posts one completion.
        assert!(reactor.inner.ring.submission().is_empty());
        reactor.inner.ring.submitter().submit_and_wait(1).unwrap();
        let err = reactor
            .inner
            .ring
            .submitter()
            .register_sync_cancel(None, CancelBuilder::user_data(Reactor::NOTIFY))
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    // Keep the kernel ring, but destroy the reactor before completing the read.
    // Reclaim the deliberately retained allocation only after its CQE arrives.
    fn complete_retained_notifier(mut reactor: Reactor) {
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Retained
        );
        let buf = ptr::from_ref(reactor.inner.notify_read.buf.as_deref().unwrap()).cast_mut();
        let notify = reactor.notifier.handle();
        let inner = Rc::get_mut(&mut reactor.inner).unwrap();
        let ring = mem::replace(&mut inner.ring, IoUring::new(1).unwrap());
        let weak = Rc::downgrade(&reactor.inner);
        drop(reactor);
        assert!(weak.upgrade().is_none());

        notify.notify().unwrap();
        ring.submitter().submit_and_wait(1).unwrap();
        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        let entry = cq.next().unwrap();
        assert_eq!(entry.user_data(), Reactor::NOTIFY);
        assert_eq!(entry.result(), 8);
        // SAFETY: `buf` is the allocation the reactor retained; its read completed with the CQE above.
        let buf = unsafe { Box::from_raw(buf) };
        // SAFETY: The read completed, so the kernel no longer writes to the cell.
        assert_eq!(unsafe { *buf.get() }, 1);
    }

    /// STATEMENT: A reactor whose notifier read was never submitted is freed when dropped.
    ///
    /// HOW:
    /// - Create a reactor; expect the notifier read queued and the submission queue empty.
    /// - Drop the reactor; expect its inner state freed.
    #[test]
    fn notifier_unrun() {
        let Ok(reactor) = Reactor::new(8) else {
            return;
        };
        assert_eq!(reactor.inner.notify_read.state.get(), NotifierState::Queued);
        assert!(reactor.inner.ring.submission().is_empty());
        let weak = Rc::downgrade(&reactor.inner);
        drop(reactor);
        assert!(weak.upgrade().is_none());
    }

    /// STATEMENT: Debug output shows the ring descriptor, the handler counter and the
    /// notifier of the reactor, and the operation id batch of a handler api.
    ///
    /// HOW:
    /// - Format a new reactor and check its fields.
    /// - Register two handlers, format their apis and check the second batch starts at
    ///   `1 << BATCH`, then check the reactor reports the new handler counter.
    #[test]
    fn debug_output() {
        struct Noop;
        impl Handler for Noop {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {}
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Ok(reactor) = Reactor::new(4) else {
            return;
        };
        let s = format!("{reactor:?}");
        assert!(
            s.starts_with(&format!(
                "Reactor {{ fd: {}, hid: Cell {{ value: 0 }}, notifier: Notifier {{",
                reactor.fd
            )),
            "{s}"
        );

        let apis = RefCell::new(Vec::new());
        for _ in 0..2 {
            reactor.register(|api| {
                apis.borrow_mut().push(format!("{api:?}"));
                Box::new(Noop)
            });
        }
        assert_eq!(
            apis.into_inner(),
            [
                "ReactorApi { batch: 0 }".to_string(),
                format!("ReactorApi {{ batch: {} }}", 1u64 << Reactor::BATCH),
            ]
        );
        assert!(format!("{reactor:?}").contains("hid: Cell { value: 2 }"));
    }

    /// STATEMENT: A notifier read held back by a full submission queue stops without a cancel,
    /// and the queued changes still run.
    ///
    /// HOW:
    /// - Queue 8 changes on a ring of 4 entries; expect the read still queued and 4 changes
    ///   left over.
    /// - Stop the notifier with a cancel that panics; expect it stopped.
    /// - Flush and submit; expect all 8 changes completed and nothing left to submit.
    #[test]
    fn notifier_stops_with_full_queue() {
        let Ok(reactor) = Reactor::new(4) else {
            return;
        };
        let sq = reactor.inner.ring.submission();
        for _ in 0..8 {
            reactor.inner.push(|en| {
                Nop::new(en).user_data(Reactor::CANCEL);
            });
        }
        assert!(reactor.apply_changes(sq));
        assert_eq!(reactor.inner.notify_read.state.get(), NotifierState::Queued);
        reactor
            .inner
            .notify_read
            .stop_with(&reactor.inner.ring, || panic!("read was not queued"));
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Stopped
        );
        assert_eq!(sq.len(), 4);
        // SAFETY: No other borrow of the change queue is live.
        assert_eq!(unsafe { &*reactor.inner.changes.get() }.len(), 4);

        let submitter = reactor.inner.ring.submitter();
        reactor.flush(sq, &submitter);
        submitter.submit_and_wait(8).unwrap();
        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { reactor.inner.ring.completion_shared() };
        cq.sync();
        assert_eq!(cq.len(), 8);
        assert!(cq.all(|e| e.user_data() == Reactor::CANCEL));
        assert!(reactor.inner.ring.submission().is_empty());
    }

    /// STATEMENT: Stopping a submitted notifier read cancels it, and its canceled completion
    /// does not re-arm it.
    ///
    /// HOW:
    /// - Submit the notifier read and stop it; expect it stopped and no read in the kernel.
    /// - Poll the canceled completion and stop again with a cancel that panics.
    /// - Expect no new submissions.
    #[test]
    fn notifier_cancellation_does_not_rearm() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
        reactor.inner.ring.submitter().submit().unwrap();
        reactor.inner.notify_read.stop(&reactor.inner.ring);
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Stopped
        );
        assert_notifier_canceled(&reactor);

        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { reactor.inner.ring.completion_shared() };
        cq.sync();
        assert!(!reactor.poll_completions(&mut cq).1);
        cq.sync();
        reactor
            .inner
            .notify_read
            .stop_with(&reactor.inner.ring, || panic!("cleanup was not idempotent"));
        assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
        assert!(reactor.inner.ring.submission().is_empty());
    }

    /// STATEMENT: Stopping a notifier whose read already completed leaves no read in the
    /// kernel.
    ///
    /// HOW:
    /// - Submit the notifier read and complete it with a notification.
    /// - Stop the notifier; expect it stopped and no read in the kernel.
    #[test]
    fn notifier_completed_before_cleanup() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
        reactor.notifier.handle().notify().unwrap();
        reactor.inner.ring.submitter().submit_and_wait(1).unwrap();
        assert_eq!(reactor.inner.notify_read.state.get(), NotifierState::Active);
        reactor.inner.notify_read.stop(&reactor.inner.ring);
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Stopped
        );
        assert_notifier_canceled(&reactor);
    }

    /// STATEMENT: Stopping a notifier whose read is in the submission queue retains its buffer
    /// until the read completes.
    ///
    /// HOW:
    /// - Queue the notifier read without submitting it, then stop the notifier.
    /// - Expect the read still queued and the notifier retained.
    /// - Drop the reactor, notify, and expect the read to complete into the retained buffer.
    #[test]
    fn notifier_queued_read_is_retained() {
        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
        assert_eq!(reactor.inner.ring.submission().len(), 1);
        reactor.inner.notify_read.stop(&reactor.inner.ring);
        assert_eq!(reactor.inner.ring.submission().len(), 1);
        complete_retained_notifier(reactor);
    }

    /// STATEMENT: When cancelling the notifier read fails, its buffer is retained until the
    /// read completes.
    ///
    /// HOW:
    /// - For `EINVAL`, `ETIME` and `EIO`, submit the read and stop it with a failing cancel.
    /// - Stop again with a cancel that panics; expect no new submissions.
    /// - Drop the reactor, notify, and expect the read to complete into the retained buffer.
    #[test]
    fn notifier_cancel_failure_is_retained() {
        for errno in [libc::EINVAL, libc::ETIME, libc::EIO] {
            let Some(reactor) = sync_cancel_reactor() else {
                return;
            };
            assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
            reactor.inner.ring.submitter().submit().unwrap();
            reactor
                .inner
                .notify_read
                .stop_with(&reactor.inner.ring, || {
                    Err(io::Error::from_raw_os_error(errno))
                });
            reactor
                .inner
                .notify_read
                .stop_with(&reactor.inner.ring, || panic!("cleanup was not idempotent"));
            assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
            assert!(reactor.inner.ring.submission().is_empty());
            complete_retained_notifier(reactor);
        }
    }

    /// STATEMENT: When the driver fails, the notifier read is retained and handlers are
    /// still cleaned up and released.
    ///
    /// HOW:
    /// - Replace the ring with a disabled one, register a handler with nothing in flight.
    /// - Run the driver and check it fails with `EBADFD`.
    /// - Check `cleanup` was called, the handler was dropped and the notifier is retained.
    /// - Enable the ring and complete the retained notifier read.
    #[test]
    fn notifier_driver_error() {
        let Ok(mut reactor) = Reactor::new(8) else {
            return;
        };
        // A disabled ring rejects submission with EBADFD.
        let inner = Rc::get_mut(&mut reactor.inner).unwrap();
        inner.ring = IoUring::builder().setup_r_disabled().build(8).unwrap();
        reactor.fd = inner.ring.as_raw_fd();
        let events = Rc::new(Events::default());
        reactor.register(|_| {
            Box::new(Tracked {
                events: events.clone(),
                in_flight: 0,
            })
        });
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let err = ntex_rt::Driver::run(&reactor, &rt).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBADFD));
        // handlers are released on the error path too
        assert!(events.cleanup.get());
        assert_eq!(Rc::strong_count(&events), 1);
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Retained
        );
        drop(rt);

        reactor
            .inner
            .ring
            .submitter()
            .register_enable_rings()
            .unwrap();
        complete_retained_notifier(reactor);
    }

    /// STATEMENT: When a handler panics during dispatch, the notifier read is cancelled before
    /// the panic leaves the driver.
    ///
    /// HOW:
    /// - Register a handler that submits a `Nop` and panics on its second completion.
    /// - Run the driver; expect a panic and the notifier stopped with no read in the kernel.
    /// - Expect the notifier completion to be `ECANCELED`, and no read in the ring's `fdinfo`
    ///   after the reactor is dropped.
    #[test]
    fn notifier_handler_panic() {
        struct Panicking {
            api: ReactorApi,
            seen: bool,
        }

        impl Handler for Panicking {
            fn completed(&mut self, _: usize, _: u32, _: io::Result<usize>) {
                assert!(!self.seen, "unwind with the rearmed notifier in flight");
                self.seen = true;
                // SAFETY: `Nop` entries reference no memory or descriptors.
                unsafe {
                    self.api.submit(0, |en| {
                        Nop::new(en);
                    });
                }
            }
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        let duplicate =
            // SAFETY: `dup` returned a fresh descriptor that nothing else owns.
            unsafe { OwnedFd::from_raw_fd(syscall!(libc::dup(reactor.as_raw_fd())).unwrap()) };
        let notify = reactor.notifier.handle();
        reactor.register(|api| {
            // SAFETY: `Nop` entries reference no memory or descriptors.
            unsafe {
                api.submit(0, |en| {
                    Nop::new(en);
                });
            }
            Box::new(Panicking { api, seen: false })
        });
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                rt.block_on(pending::<()>(), &reactor);
            }))
            .is_err()
        );
        assert_eq!(
            reactor.inner.notify_read.state.get(),
            NotifierState::Stopped
        );
        assert_notifier_canceled(&reactor);
        drop(rt);
        // SAFETY: The test is the only consumer of the completion queue.
        let mut cq = unsafe { reactor.inner.ring.completion_shared() };
        cq.sync();
        let entry = cq.next().unwrap();
        assert_eq!(entry.user_data(), Reactor::NOTIFY);
        assert_eq!(entry.result(), -libc::ECANCELED);
        drop(cq);
        drop(reactor);
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", duplicate.as_raw_fd()))
            .unwrap();
        assert!(!info.contains("op=22,"), "{info}");
        drop(notify);
        drop(duplicate);
    }

    /// STATEMENT: The notifier read re-arms after each wake, so repeated wakes from another
    /// thread all reach the runtime.
    ///
    /// HOW:
    /// - Wake the runtime three times from another thread, 50 ms apart.
    /// - Expect the blocked future to see all three wakes and finish.
    #[test]
    fn notifier_rearms() {
        use std::sync::{Mutex, atomic::AtomicUsize, atomic::Ordering};
        use std::{future::poll_fn, task::Poll, task::Waker, time::Duration};

        let Ok(reactor) = Reactor::new(8) else {
            return;
        };
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let cnt = Arc::new(AtomicUsize::new(0));
        let waker = Arc::new(Mutex::new(None::<Waker>));

        let (cnt2, waker2) = (cnt.clone(), waker.clone());
        let th = std::thread::spawn(move || {
            for i in 1..=3 {
                // the driver blocks on the ring before each wake
                std::thread::sleep(Duration::from_millis(50));
                cnt2.store(i, Ordering::Release);
                if let Some(w) = waker2.lock().unwrap().take() {
                    w.wake();
                }
            }
        });

        let mut seen = 0;
        rt.block_on(
            poll_fn(|cx| {
                *waker.lock().unwrap() = Some(cx.waker().clone());
                let n = cnt.load(Ordering::Acquire);
                if n > seen {
                    seen = n;
                }
                if seen == 3 { Poll::Ready(()) } else { Poll::Pending }
            }),
            &reactor,
        );
        th.join().unwrap();
    }
}
