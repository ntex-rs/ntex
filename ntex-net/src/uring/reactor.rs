use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};
use std::{collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_io_uring::cqueue::{self, Entry as CEntry, more};
use ntex_io_uring::opcode::{AsyncCancel, AsyncCancel2};
use ntex_io_uring::squeue::{Entry as SEntry, PushError, SubmissionQueue};
use ntex_io_uring::types::{CancelBuilder, SubmitArgs, Timespec};
use ntex_io_uring::{IoUring, Probe, Submitter};
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
    /// Check if the ring was set up with `IORING_SETUP_SINGLE_ISSUER`, so the
    /// kernel is 6.0 or greater and supports `IORING_RECVSEND_POLL_FIRST` (5.19)
    pub fn is_new(&self) -> bool {
        self.inner.flags.get().contains(Flags::NEW)
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

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        /// Ring set up with `IORING_SETUP_SINGLE_ISSUER`, kernel 6.0 or greater
        const NEW      = 0b0000_0001;
    }
}

struct ReactorInner {
    probe: Probe,
    flags: Cell<Flags>,
    ring: IoUring<SEntry, CEntry>,
    changes: UnsafeCell<VecDeque<mem::MaybeUninit<SEntry>>>,
    notify_read: NotifierRead,
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
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
    const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
    /// Completions handled before running tasks. Smaller chunks keep
    /// sockets hot in the cache between read completion and write, about
    /// 15% faster than handling the whole queue at once.
    const CQES_PER_POLL: usize = 16;

    /// Create io-uring driver
    pub fn new(capacity: u32) -> io::Result<Self> {
        let build = |no_sqarray: bool| {
            let mut builder = IoUring::builder();
            builder
                .setup_coop_taskrun()
                .setup_taskrun_flag()
                .setup_single_issuer()
                .setup_defer_taskrun();
            if no_sqarray {
                // available since 6.6
                builder.setup_no_sqarray();
            }
            builder.build(capacity)
        };

        // Create ring
        let (new, ring) = if let Ok(ring) = build(true) {
            log::info!(
                "New io-uring driver with single-issuer, coop-taskrun, taskrun-flag, defer-taskrun, no-sqarray"
            );
            (true, ring)
        } else if let Ok(ring) = build(false) {
            log::info!(
                "New io-uring driver with single-issuer, coop-taskrun, taskrun-flag, defer-taskrun"
            );
            (true, ring)
        } else if let Ok(ring) = IoUring::builder().setup_single_issuer().build(capacity) {
            log::info!("New io-uring driver with single-issuer");
            (true, ring)
        } else {
            let ring = IoUring::builder().build(capacity)?;
            log::info!("New io-uring driver");
            (false, ring)
        };

        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;

        // Avoid the per-call ring fd lookup in io_uring_enter, available since 5.18.
        // The registration is per-thread, the reactor is created and driven on the same thread.
        if let Err(e) = ring.submitter().register_ring_fd() {
            log::debug!("Cannot register io-uring ring fd: {e}");
        }

        let fd = ring.as_raw_fd();
        let inner = Rc::new(ReactorInner {
            ring,
            probe,
            flags: Cell::new(if new { Flags::NEW } else { Flags::empty() }),
            changes: UnsafeCell::new(VecDeque::with_capacity(32)),
            notify_read: NotifierRead::new(),
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
        let mut handlers = self.handlers.take().unwrap_or_default();
        handlers.push(HandlerItem {
            hnd: f(ReactorApi {
                batch: id << Self::BATCH,
                inner: self.inner.clone(),
            }),
            modified: false,
        });
        self.handlers.set(Some(handlers));
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
        // Operations queued during the last turn (`Close` of dropped
        // streams in particular) have not reached the kernel yet.
        self.flush(sq, submitter);

        let canceled = submitter.register_sync_cancel(None, CancelBuilder::any());
        self.inner.notify_read.canceled(ring, &canceled);
        if let Err(e) = canceled {
            log::error!("Cannot cancel in-flight operations: {e:?}");
            if e.raw_os_error() == Some(libc::EINVAL) {
                // sync cancel is available since 6.0, cancel of any request since 5.19
                self.inner.push(|en| {
                    AsyncCancel2::new(en, CancelBuilder::any()).user_data(Self::CANCEL);
                });
                self.flush(sq, submitter);
            }
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
    /// specific handler. Returns `true` if synced completions remain.
    ///
    /// The caller must sync the queue before and after.
    fn poll_completions(&self, cq: &mut cqueue::CompletionQueue<'_, CEntry>) -> bool {
        let mut has_more = false;

        if !cqueue::CompletionQueue::<'_, _>::is_empty(cq) {
            let mut handlers = self.handlers.take().unwrap();
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
            self.handlers.set(Some(handlers));
        }
        has_more
    }
}

impl AsRawFd for Reactor {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl crate::Reactor for Reactor {
    fn tcp_connect(&self, addr: net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let addr = SockAddr::from(addr);
        let result = Socket::new(addr.domain(), Type::STREAM, Some(Protocol::TCP))
            .and_then(crate::helpers::prep_tcp_socket)
            .and_then(crate::helpers::prep_socket)
            .map(move |sock| (addr, sock));

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => super::connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        let result = SockAddr::unix(addr).and_then(|addr| {
            Socket::new(addr.domain(), Type::STREAM, None)
                .and_then(crate::helpers::prep_socket)
                .map(move |sock| (addr, sock))
        });

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => super::connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
        stream.set_nodelay(true)?;

        Ok(Io::new(
            TcpStream(
                crate::helpers::prep_socket(Socket::from(stream))?,
                StreamOps::get(self),
            ),
            cfg,
        ))
    }

    #[cfg(unix)]
    fn from_unix_stream(&self, stream: OsUnixStream, cfg: SharedCfg) -> io::Result<Io> {
        Ok(Io::new(
            UnixStream(
                crate::helpers::prep_socket(Socket::from(stream))?,
                StreamOps::get(self),
            ),
            cfg,
        ))
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
        let result = loop {
            // tasks woken until the runtime is polled do not need to notify
            rt.awake();
            cq.sync();

            // Run tasks after each chunk of completions, so a task writes
            // to its socket while the socket is still hot in the cache.
            // All chunks belong to one runtime turn, and their submissions
            // share one `io_uring_enter` unless the submission queue fills.
            // A turn handles at most one queue of completions, a flush may
            // post more.
            let mut chunks = cq.capacity().div_ceil(Self::CQES_PER_POLL);
            let more_cqes = loop {
                if !self.poll_completions(&mut cq) {
                    break false;
                }
                chunks -= 1;
                if chunks == 0 {
                    break true;
                }
                if sq.is_full() {
                    // flush the full queue without ending the turn; consumed
                    // entries are published first, the enter may post more
                    cq.sync();
                    if submitter.submit().is_err() {
                        // the end of the turn retries and reports the error
                        break true;
                    }
                    self.apply_changes(sq);
                    // SAFETY: No other borrow of the change queue is live.
                    if unsafe { !(*self.inner.changes.get()).is_empty() } {
                        break true;
                    }
                }
                rt.run_ready();
            };
            // publish the consumed entries, the kernel would count them as
            // pending completions and return from the next wait immediately
            cq.sync();

            let more_tasks = match rt.poll() {
                PollResult::Pending => false,
                PollResult::PollAgain => true,
                PollResult::Ready => break Ok(()),
            };
            let more_changes = self.apply_changes(sq);

            // submit publishes the queue tail, and skips the syscall if
            // there is nothing to submit and no deferred completions to post.
            // Without deferred task running, completions may arrive during
            // the turn, waiting would return immediately.
            let result = if more_changes || more_tasks || more_cqes || {
                cq.sync();
                !cqueue::CompletionQueue::<'_, _>::is_empty(&cq)
            } {
                submitter.submit()
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
        };

        self.shutdown(sq, &submitter, &mut cq);
        result
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
            .field("nodifier", &self.notifier)
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

    use ntex_io_uring::opcode::{Nop, Read};
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
    /// and reports whether more remain.
    ///
    /// HOW:
    /// - Submit `CQES_PER_POLL + 4` `Nop` operations to a counting handler and wait for them.
    /// - Sync once, poll and check `CQES_PER_POLL` completions were handled and `true` returned.
    /// - Poll again and check the rest were handled, `false` returned and the queue is empty.
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
        assert!(reactor.poll_completions(&mut cq));
        assert_eq!(count.get(), Reactor::CQES_PER_POLL);
        assert!(!reactor.poll_completions(&mut cq));
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
        assert!(reactor.inner.ring.submission().is_empty());
        reactor.inner.ring.submitter().submit().unwrap();
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
        assert!(!reactor.poll_completions(&mut cq));
        cq.sync();
        reactor
            .inner
            .notify_read
            .stop_with(&reactor.inner.ring, || panic!("cleanup was not idempotent"));
        assert!(!reactor.apply_changes(reactor.inner.ring.submission()));
        assert!(reactor.inner.ring.submission().is_empty());
    }

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
