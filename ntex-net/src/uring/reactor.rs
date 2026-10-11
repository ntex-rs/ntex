use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc, sync::Arc};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_io_uring::cqueue::{self, Entry as CEntry, more};
use ntex_io_uring::opcode::{AsyncCancel, Read};
use ntex_io_uring::squeue::{Entry as SEntry, PushError, SubmissionQueue};
use ntex_io_uring::{IoUring, Probe, Submitter, types::CancelBuilder, types::Fd};
use ntex_rt::{DriverType, Notify, PollResult, Runtime, syscall};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{TcpStream, UnixStream, stream::StreamOps};
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
        const NOTIFIER = 0b0000_0010;
    }
}

struct ReactorInner {
    probe: Probe,
    flags: Cell<Flags>,
    ring: IoUring<SEntry, CEntry>,
    changes: UnsafeCell<VecDeque<mem::MaybeUninit<SEntry>>>,
    // eventfd counter, written by the in-flight notifier read
    notify_buf: UnsafeCell<u64>,
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

    /// Read the notifier eventfd, the completion wakes the driver
    fn arm_notifier(&self, fd: RawFd) {
        self.push(|en| {
            Read::new(en, Fd(fd), self.notify_buf.get().cast(), 8).user_data(Reactor::NOTIFY);
        });
    }
}

impl Reactor {
    const NOTIFY: u64 = u64::MAX;
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
            notify_buf: UnsafeCell::new(0),
        });

        // Remote notifier
        let notifier = Notifier::new()?;
        inner.arm_notifier(notifier.as_raw_fd());

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
            if changes.is_empty() {
                false
            } else {
                // safety: "changes" contains only initialized entries
                while let Some(entry) = changes.front() {
                    let res = sq.push(|en| ptr::copy_nonoverlapping(entry.as_ptr(), en, 1));
                    if res.is_err() {
                        break;
                    }
                    changes.pop_front();
                }

                !changes.is_empty()
            }
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
                    Self::NOTIFY => {
                        // the read resets the eventfd counter
                        let res = entry.result();
                        assert!(
                            res >= 0 || res == -libc::EINTR || res == -libc::ECANCELED,
                            "cannot read notifier: {}",
                            io::Error::from_raw_os_error(-res)
                        );
                        self.inner.arm_notifier(self.notifier.as_raw_fd());
                    }
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
            // Operations queued during the last turn (`Close` of dropped
            // streams in particular) have not reached the kernel yet.
            self.flush(sq, &submitter);

            // Handlers release buffers and sockets referenced by in-flight
            // operations, the kernel must be done with all of them first.
            if let Err(e) = submitter.register_sync_cancel(None, CancelBuilder::any()) {
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

#[derive(Debug)]
pub(crate) struct Notifier {
    fd: Arc<OwnedFd>,
}

impl Notifier {
    /// Create a new notifier.
    pub(crate) fn new() -> io::Result<Self> {
        // io-uring fails reads of non-blocking files with EAGAIN instead of
        // waiting for data, a write blocks only on the counter overflow
        let fd = syscall!(libc::eventfd(0, libc::EFD_CLOEXEC))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd: Arc::new(fd) })
    }

    pub(crate) fn handle(&self) -> NotifyHandle {
        NotifyHandle::new(self.fd.clone())
    }
}

impl AsRawFd for Notifier {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[derive(Clone, Debug)]
/// A notify handle to the driver.
pub(crate) struct NotifyHandle {
    fd: Arc<OwnedFd>,
}

impl NotifyHandle {
    pub(crate) fn new(fd: Arc<OwnedFd>) -> Self {
        Self { fd }
    }
}

impl Notify for NotifyHandle {
    /// Notify the driver.
    fn notify(&self) -> io::Result<()> {
        let data = 1u64;
        syscall!(libc::write(
            self.fd.as_raw_fd(),
            (&raw const data).cast(),
            std::mem::size_of::<u64>(),
        ))?;
        Ok(())
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
    use ntex_io_uring::opcode::Nop;

    use super::*;

    #[test]
    fn apply_wrapped_changes() {
        let Ok(reactor) = Reactor::new(4) else {
            return;
        };
        let ring = &reactor.inner.ring;
        let sq = ring.submission();
        // submit the notifier
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
