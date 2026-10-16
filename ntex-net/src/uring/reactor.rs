use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, RawFd};
use std::{collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_io_uring::cqueue::{self, Entry as CEntry, more};
use ntex_io_uring::opcode::AsyncCancel;
use ntex_io_uring::squeue::{Entry as SEntry, PushError, SubmissionQueue};
use ntex_io_uring::{IoUring, Probe, Submitter, types::CancelBuilder, types::Fd};
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

    /// Clean up the handle before dropping the driver.
    fn cleanup(&mut self);
}

/// Api for io-uring reactor.
pub struct ReactorApi {
    batch: u64,
    inner: Rc<ReactorInner>,
}

impl ReactorApi {
    #[inline]
    /// Check if kernel ver 6.1 or greater
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
    /// `f` initializes the submission entry in place with an opcode.
    pub fn submit<F>(&self, id: u32, f: F)
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

    #[inline]
    /// Attempt to sync cancel all requests.
    pub fn cancel_all_sync(&self, fd: Fd) -> io::Result<()> {
        self.inner
            .ring
            .submitter()
            .register_sync_cancel(None, CancelBuilder::fd(fd).all())
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
        const NEW      = 0b0000_0001;
    }
}

struct ReactorInner {
    probe: Probe,
    flags: Cell<Flags>,
    ring: IoUring<SEntry, CEntry>,
    changes: UnsafeCell<VecDeque<mem::MaybeUninit<SEntry>>>,
    notify_read: NotifierRead,
}

impl ReactorInner {
    fn push<F>(&self, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
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

    /// Handle ring completions, forward changes to specific handler
    fn poll_completions(&self, cq: &mut cqueue::CompletionQueue<'_, CEntry>) {
        cq.sync();

        if !cqueue::CompletionQueue::<'_, _>::is_empty(cq) {
            let mut handlers = self.handlers.take().unwrap();
            for entry in &mut *cq {
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
            // publish the consumed entries, the kernel would count them as
            // pending completions and return from the next wait immediately
            cq.sync();

            for h in handlers.iter_mut() {
                h.tick();
            }
            self.handlers.set(Some(handlers));
        }
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
        let mut cq = unsafe { ring.completion_shared() };
        let submitter = ring.submitter();
        let result = loop {
            // tasks woken until the runtime is polled do not need to notify
            rt.awake();
            self.poll_completions(&mut cq);

            let more_tasks = match rt.poll() {
                PollResult::Pending => false,
                PollResult::PollAgain => true,
                PollResult::Ready => break Ok(()),
            };
            let more_changes = self.apply_changes(sq);

            // submit publishes the queue tail, and skips the syscall if
            // there is nothing to submit and no deferred completions to post
            let result = if more_changes || more_tasks {
                submitter.submit()
            } else {
                submitter.submit_and_wait(1)
            };

            if let Err(e) = result {
                match e.raw_os_error() {
                    Some(libc::ETIME | libc::EBUSY | libc::EAGAIN | libc::EINTR) => {
                        log::info!("Ring submit interrupted, {e:?}");
                    }
                    _ => break Err(e),
                }
            }
        };

        // cleanup handlers
        if result.is_ok() {
            self.inner.notify_read.stop_arming();
            // Operations queued during the last turn (`Close` of dropped
            // streams in particular) have not reached the kernel yet.
            self.flush(sq, &submitter);

            // Handlers release buffers and sockets referenced by in-flight
            // operations, the kernel must be done with all of them first.
            let canceled = submitter.register_sync_cancel(None, CancelBuilder::any());
            self.inner.notify_read.canceled(ring, &canceled);
            if let Err(e) = canceled {
                log::error!("Cannot cancel in-flight operations: {e:?}");
            }

            for mut h in self.handlers.take().unwrap().into_iter() {
                h.hnd.cleanup();
            }
        }

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
    use std::future::pending;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use ntex_io_uring::opcode::Nop;
    use ntex_rt::syscall;

    use super::notifier::NotifierState;
    use super::*;

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

        // queued entries wrap around the end of the deque storage
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

        assert!(!reactor.apply_changes(sq));
        sq.sync();
        assert!(changes.is_empty());
        assert_eq!(sq.len(), 4);

        ring.submitter().submit_and_wait(4).unwrap();
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        let ids: Vec<_> = cq.map(|e| e.user_data()).collect();
        assert_eq!(ids, [0, 1, 2, 3]);
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
        let mut cq = unsafe { ring.completion_shared() };
        cq.sync();
        let entry = cq.next().unwrap();
        assert_eq!(entry.user_data(), Reactor::NOTIFY);
        assert_eq!(entry.result(), 8);
        let buf = unsafe { Box::from_raw(buf) };
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
        assert_eq!(unsafe { &*reactor.inner.changes.get() }.len(), 4);

        let submitter = reactor.inner.ring.submitter();
        reactor.flush(sq, &submitter);
        submitter.submit_and_wait(8).unwrap();
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

        let mut cq = unsafe { reactor.inner.ring.completion_shared() };
        reactor.poll_completions(&mut cq);
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

    #[test]
    fn notifier_driver_error() {
        let Ok(mut reactor) = Reactor::new(8) else {
            return;
        };
        // A disabled ring rejects submission with EBADFD.
        let inner = Rc::get_mut(&mut reactor.inner).unwrap();
        inner.ring = IoUring::builder().setup_r_disabled().build(8).unwrap();
        reactor.fd = inner.ring.as_raw_fd();
        let rt = Runtime::new(ntex_rt::Driver::handle(&reactor));
        let err = ntex_rt::Driver::run(&reactor, &rt).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBADFD));
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
                self.api.submit(0, |en| {
                    Nop::new(en);
                });
            }
            fn canceled(&mut self, _: usize) {}
            fn tick(&mut self) {}
            fn cleanup(&mut self) {}
        }

        let Some(reactor) = sync_cancel_reactor() else {
            return;
        };
        let duplicate =
            unsafe { OwnedFd::from_raw_fd(syscall!(libc::dup(reactor.as_raw_fd())).unwrap()) };
        let notify = reactor.notifier.handle();
        reactor.register(|api| {
            api.submit(0, |en| {
                Nop::new(en);
            });
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
