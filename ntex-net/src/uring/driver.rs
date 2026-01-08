use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{cmp, collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc, sync::Arc};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use io_uring::cqueue::{self, Entry as CEntry, more};
use io_uring::opcode::{AsyncCancel, PollAdd};
use io_uring::squeue::{Entry as SEntry, SubmissionQueue};
use io_uring::{IoUring, Probe, types::Fd};
use ntex_io::Io;
use ntex_rt::{DriverType, Notify, PollResult, Runtime, syscall};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{TcpStream, UnixStream, stream::StreamOps};
use crate::channel::Receiver;

pub trait Handler {
    /// Operation is completed
    fn completed(&mut self, id: usize, flags: u32, result: io::Result<usize>);

    /// Operation is canceled
    fn canceled(&mut self, id: usize);

    /// Driver turn is completed
    fn tick(&mut self);

    /// Cleanup before drop
    fn cleanup(&mut self);
}

pub struct DriverApi {
    batch: u64,
    inner: Rc<DriverInner>,
}

impl DriverApi {
    #[inline]
    /// Check if kernel ver 6.1 or greater
    pub fn is_new(&self) -> bool {
        self.inner.flags.get().contains(Flags::NEW)
    }

    fn submit_inner<F>(&self, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        unsafe {
            let changes = &mut *self.inner.changes.get();
            let sq = self.inner.ring.submission();
            if !changes.is_empty() || sq.is_full() {
                changes.push_back(mem::MaybeUninit::uninit());
                let entry = changes.back_mut().unwrap();
                ptr::write_bytes(entry.as_mut_ptr(), 0, 1);
                f(entry.assume_init_mut());
            } else {
                sq.push_inline(f).expect("Queue size is checked");
            }
        }
    }

    #[inline]
    /// Submit request to the driver.
    pub fn submit(&self, id: u32, entry: SEntry) {
        self.submit_inner(|en| {
            *en = entry;
            en.set_user_data(id as u64 | self.batch);
        });
    }

    #[inline]
    /// Submit request to the driver.
    pub fn submit_inline<F>(&self, id: u32, f: F)
    where
        F: FnOnce(&mut SEntry),
    {
        self.submit_inner(|en| {
            f(en);
            en.set_user_data(id as u64 | self.batch);
        });
    }

    #[inline]
    /// Attempt to cancel an already issued request.
    pub fn cancel(&self, id: u32) {
        self.submit_inner(|en| {
            *en = AsyncCancel::new(id as u64 | self.batch)
                .build()
                .user_data(Driver::CANCEL);
        });
    }

    /// Get whether a specific io-uring opcode is supported.
    pub fn is_supported(&self, opcode: u8) -> bool {
        self.inner.probe.is_supported(opcode)
    }
}

/// Low-level driver of io-uring.
pub struct Driver {
    fd: RawFd,
    hid: Cell<u64>,
    notifier: Notifier,
    #[allow(clippy::box_collection)]
    handlers: Cell<Option<Box<Vec<HandlerItem>>>>,
    inner: Rc<DriverInner>,
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

struct DriverInner {
    probe: Probe,
    flags: Cell<Flags>,
    ring: IoUring<SEntry, CEntry>,
    changes: UnsafeCell<VecDeque<mem::MaybeUninit<SEntry>>>,
}

impl Driver {
    const NOTIFY: u64 = u64::MAX;
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// Create io-uring driver
    pub fn new(capacity: u32) -> io::Result<Self> {
        // Create ring
        let (new, ring) = if let Ok(ring) = IoUring::builder()
            .setup_coop_taskrun()
            .setup_single_issuer()
            .setup_defer_taskrun()
            .build(capacity)
        {
            log::info!(
                "New io-uring driver with single-issuer, coop-taskrun, defer-taskrun"
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

        // Remote notifier
        let notifier = Notifier::new()?;
        unsafe {
            let sq = ring.submission();
            sq.push(
                &PollAdd::new(Fd(notifier.as_raw_fd()), libc::POLLIN as _)
                    .multi(true)
                    .build()
                    .user_data(Self::NOTIFY),
            )
            .expect("the squeue sould not be full");
            sq.sync();
        }

        let fd = ring.as_raw_fd();
        let inner = Rc::new(DriverInner {
            ring,
            probe,
            flags: Cell::new(if new { Flags::NEW } else { Flags::empty() }),
            changes: UnsafeCell::new(VecDeque::with_capacity(32)),
        });

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
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();
        handlers.push(HandlerItem {
            hnd: f(DriverApi {
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
                let num = cmp::min(changes.len(), sq.capacity() - sq.len());
                let (s1, s2) = changes.as_slices();
                let s1_num = cmp::min(s1.len(), num);
                if s1_num > 0 {
                    // safety: "changes" contains only initialized entries
                    sq.push_multiple(mem::transmute::<
                        &[mem::MaybeUninit<SEntry>],
                        &[SEntry],
                    >(&s1[0..s1_num]))
                        .unwrap();
                } else if !s2.is_empty() {
                    let s2_num = cmp::min(s2.len(), num - s1_num);
                    if s2_num > 0 {
                        sq.push_multiple(mem::transmute::<
                            &[mem::MaybeUninit<SEntry>],
                            &[SEntry],
                        >(&s2[0..s2_num]))
                            .unwrap();
                    }
                }
                changes.drain(0..num);

                !changes.is_empty()
            }
        }
    }

    /// Handle ring completions, forward changes to specific handler
    fn poll_completions(
        &self,
        cq: &mut cqueue::CompletionQueue<'_, CEntry>,
        sq: &SubmissionQueue<'_, SEntry>,
    ) {
        cq.sync();

        if !cqueue::CompletionQueue::<'_, _>::is_empty(cq) {
            let mut handlers = self.handlers.take().unwrap();
            for entry in cq {
                let user_data = entry.user_data();
                match user_data {
                    Self::CANCEL => {}
                    Self::NOTIFY => {
                        let flags = entry.flags();
                        self.notifier.clear().expect("cannot clear notifier");

                        // re-submit notifier fd
                        if !more(flags) {
                            unsafe {
                                sq.push(
                                    &PollAdd::new(
                                        Fd(self.notifier.as_raw_fd()),
                                        libc::POLLIN as _,
                                    )
                                    .multi(true)
                                    .build()
                                    .user_data(Self::NOTIFY),
                                )
                            }
                            .expect("the squeue sould not be full");
                        }
                    }
                    _ => {
                        let batch =
                            ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                        let user_data = (user_data & Self::DATA_MASK) as usize;

                        let result = entry.result();
                        if result == -libc::ECANCELED {
                            handlers[batch].modified = true;
                            handlers[batch].hnd.canceled(user_data);
                        } else {
                            let result = if result < 0 {
                                Err(io::Error::from_raw_os_error(-result))
                            } else {
                                Ok(result as _)
                            };
                            handlers[batch].modified = true;
                            handlers[batch]
                                .hnd
                                .completed(user_data, entry.flags(), result);
                        }
                    }
                }
            }
            for h in handlers.iter_mut() {
                h.tick();
            }
            self.handlers.set(Some(handlers));
        }
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl crate::Reactor for Driver {
    fn tcp_connect(&self, addr: net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let addr = SockAddr::from(addr);
        let result = Socket::new(addr.domain(), Type::STREAM, Some(Protocol::TCP))
            .and_then(crate::helpers::prep_socket)
            .map(move |sock| (addr, sock));

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => {
                super::connect::ConnectOps::get(self).connect(sock, addr, cfg)
            }
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
            Ok((addr, sock)) => {
                super::connect::ConnectOps::get(self).connect(sock, addr, cfg)
            }
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

impl ntex_rt::Driver for Driver {
    /// Poll the driver and handle completed operations.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        let ring = &self.inner.ring;
        let sq = ring.submission();
        let mut cq = unsafe { ring.completion_shared() };
        let submitter = ring.submitter();
        loop {
            self.poll_completions(&mut cq, &sq);

            let more_tasks = match rt.poll() {
                PollResult::Pending => false,
                PollResult::PollAgain => true,
                PollResult::Ready => return Ok(()),
            };
            let more_changes = self.apply_changes(sq);

            // squeue has to sync after we apply all changes
            // otherwise ring wont see any change in submit call
            sq.sync();

            let result = if more_changes || more_tasks {
                submitter.submit()
            } else {
                submitter.submit_and_wait(1)
            };

            if let Err(e) = result {
                match e.raw_os_error() {
                    Some(libc::ETIME) | Some(libc::EBUSY) | Some(libc::EAGAIN)
                    | Some(libc::EINTR) => {
                        log::info!("Ring submit interrupted, {:?}", e);
                    }
                    _ => return Err(e),
                }
            }
        }
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(self.notifier.handle())
    }

    fn clear(&self) {
        for mut h in self.handlers.take().unwrap().into_iter() {
            h.hnd.cleanup()
        }
    }
}

#[derive(Debug)]
pub(crate) struct Notifier {
    fd: Arc<OwnedFd>,
}

impl Notifier {
    /// Create a new notifier.
    pub(crate) fn new() -> io::Result<Self> {
        let fd = syscall!(libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd: Arc::new(fd) })
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        loop {
            let mut buffer = [0u64];
            let res = syscall!(libc::read(
                self.fd.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                mem::size_of::<u64>()
            ));
            match res {
                Ok(len) => {
                    debug_assert_eq!(len, mem::size_of::<u64>() as isize);
                    break Ok(());
                }
                // Clear the next time
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break Ok(()),
                // Just like read_exact
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Err(e),
            }
        }
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
            &data as *const _ as *const _,
            std::mem::size_of::<u64>(),
        ))?;
        Ok(())
    }
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("fd", &self.fd)
            .field("hid", &self.hid)
            .field("nodifier", &self.notifier)
            .finish()
    }
}

impl fmt::Debug for DriverApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverApi")
            .field("batch", &self.batch)
            .finish()
    }
}
