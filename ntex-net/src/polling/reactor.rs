use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{cell::Cell, cell::UnsafeCell, fmt, io, net, rc::Rc};
use std::{collections::VecDeque, num::NonZeroUsize, time::Duration};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ::ntex_polling::{Event, Events, Notifier, PollMode, Poller};
use ntex_io::Io;
use ntex_rt::{DriverType, Notify, PollResult, Runtime};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{TcpStream, UnixStream, stream::StreamOps};
use crate::channel::Receiver;

pub trait Handler {
    /// Submitted interest, `id` is the one passed to [`ReactorApi`].
    ///
    /// Tasks run between chunks of events of one poll, so `id` may belong to
    /// a registration released by a task after the poll.
    fn event(&mut self, id: usize, event: Event);

    /// Operation submission has failed, `id` is the one passed to [`ReactorApi`].
    fn error(&mut self, id: usize, err: io::Error);

    /// Process deferred work after events and before the driver waits.
    ///
    /// Called even when no events were received.
    fn tick(&mut self);

    /// Cleanup before drop
    fn cleanup(&mut self);
}

type HandlerItem = Box<dyn Handler>;

enum Change {
    Error {
        batch: usize,
        user_data: u64,
        error: io::Error,
    },
}

#[derive(Debug)]
/// Polling reactor api.
///
/// Ids identify registrations to the handler, only their low 48 bits are
/// available.
pub struct ReactorApi {
    id: usize,
    batch: u64,
    poll: Rc<Poller>,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
}

impl ReactorApi {
    /// Attach an fd to the driver.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach(&self, fd: RawFd, id: u64, event: Event) {
        self.attach_with_mode(fd, id, event, PollMode::Oneshot);
    }

    /// Attach an fd to the driver with specific mode.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach_with_mode(&self, fd: RawFd, id: u64, mut event: Event, mode: PollMode) {
        event.key = self.key(id);
        // SAFETY: Internal stream callers keep `fd` open and detach it before closing it.
        if let Err(err) = unsafe { self.poll.add_with_mode(fd, event, mode) } {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            });
        }
    }

    /// Detach an fd from the driver.
    pub fn detach(&self, fd: RawFd, id: u64) {
        // SAFETY: Internal stream callers keep `fd` open until detachment returns.
        if let Err(err) = self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) }) {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            });
        }
    }

    /// Register interest for specified file descriptor.
    pub fn modify(&self, fd: RawFd, id: u64, event: Event) {
        self.modify_with_mode(fd, id, event, PollMode::Oneshot);
    }

    /// Register interest for specified file descriptor.
    pub fn modify_with_mode(&self, fd: RawFd, id: u64, mut event: Event, mode: PollMode) {
        event.key = self.key(id);

        let result = self
            .poll
            // SAFETY: Internal stream callers keep `fd` open during registration.
            .modify_with_mode(unsafe { BorrowedFd::borrow_raw(fd) }, event, mode);
        self.check(id, result);
    }

    /// Register interest for specified file descriptor, the change may be
    /// deferred to the next poll.
    ///
    /// On kqueue the change is submitted together with the next wait, saving
    /// a syscall. A failure of a deferred change is reported as a readable
    /// and writable event.
    pub fn modify_with_mode_deferred(&self, fd: RawFd, id: u64, mut event: Event, mode: PollMode) {
        event.key = self.key(id);

        let result = self
            .poll
            // SAFETY: Internal stream callers keep `fd` open until detachment.
            .modify_with_mode_deferred(unsafe { BorrowedFd::borrow_raw(fd) }, event, mode);
        self.check(id, result);
    }

    /// Whether the poller supports level-triggered events.
    pub fn supports_level(&self) -> bool {
        self.poll.supports_level()
    }

    fn check(&self, id: u64, result: io::Result<()>) {
        if let Err(err) = result {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            });
        }
    }

    /// Event key of `id`, ids are limited to 48 bits.
    fn key(&self, id: u64) -> usize {
        debug_assert_eq!(id & !Reactor::DATA_MASK, 0, "id {id:#x} exceeds 48 bits");
        (id | self.batch) as usize
    }

    fn change(&self, ev: Change) {
        // SAFETY: This thread-local queue is borrowed only for this non-reentrant push.
        unsafe { (*self.changes.get()).push_back(ev) };
    }

    /// Whether a failed poller operation is queued.
    #[cfg(test)]
    pub(super) fn has_changes(&self) -> bool {
        // SAFETY: Thread-local queue, no push or pop overlaps this read.
        unsafe { !(*self.changes.get()).is_empty() }
    }
}

/// Polling reactor.
///
/// Uses `epoll` or `kqueue`, depending on the platform.
pub struct Reactor {
    poll: Rc<Poller>,
    capacity: usize,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
    hid: Cell<u64>,
    #[allow(clippy::box_collection)]
    handlers: Cell<Option<Box<Vec<HandlerItem>>>>,
}

impl Reactor {
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    /// Events read per poll.
    const EVENTS_CAPACITY: u32 = 512;

    /// Events handled between task runs.
    ///
    /// Small chunks let a connection write its response while its socket
    /// state is still in cache. Handling 2048 events at once with ~1K busy
    /// connections per worker ran ~20% slower in HTTP/1 benchmarks.
    const EVENTS_PER_CHUNK: usize = 8;

    pub fn new() -> io::Result<Self> {
        Reactor::with_capacity(Self::EVENTS_CAPACITY)
    }

    pub fn with_capacity(io_queue_capacity: u32) -> io::Result<Self> {
        log::trace!("New poll driver");

        Ok(Self {
            hid: Cell::new(0),
            poll: Rc::new(Poller::new()?),
            capacity: io_queue_capacity as usize,
            changes: Rc::new(UnsafeCell::new(VecDeque::with_capacity(32))),
            handlers: Cell::new(Some(Box::new(Vec::default()))),
        })
    }

    /// Reactor type
    pub const fn tp(&self) -> DriverType {
        DriverType::Poll
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(ReactorApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self
            .handlers
            .take()
            .expect("Cannot register handler during event handling");

        let api = ReactorApi {
            id: id as usize,
            batch: id << Self::BATCH,
            poll: self.poll.clone(),
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    fn apply_changes(&self, handlers: &mut [HandlerItem]) {
        // SAFETY: Queue access is thread-local and the borrow ends before handler callbacks.
        while let Some(op) = unsafe { (*self.changes.get()).pop_front() } {
            match op {
                Change::Error {
                    batch,
                    user_data,
                    error,
                } => handlers[batch].error(user_data as usize, error),
            }
        }
    }

    fn tick(&self, handlers: &mut [HandlerItem]) {
        loop {
            self.apply_changes(handlers);
            for h in handlers.iter_mut() {
                h.tick();
            }
            // Ticks can fail poller operations, and handling those errors can
            // enqueue more deferred work. Finish both before waiting.
            // SAFETY: No queue mutation or handler callback overlaps this thread-local read.
            if unsafe { (*self.changes.get()).is_empty() } {
                break;
            }
        }
    }
}

impl AsRawFd for Reactor {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}

impl fmt::Debug for Reactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reactor")
            .field("poll", &self.poll)
            .field("capacity", &self.capacity)
            .field("hid", &self.hid)
            .finish()
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
            Ok((addr, sock)) => {
                super::connect::ConnectOps::get(self).connect(sock, &addr, cfg, false)
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
                super::connect::ConnectOps::get(self).connect(sock, &addr, cfg, true)
            }
        }
    }

    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
        stream.set_nodelay(true)?;

        Ok(Io::new(
            TcpStream(
                crate::helpers::prep_socket(Socket::from(stream))?,
                StreamOps::get(self),
                false,
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
                false,
            ),
            cfg,
        ))
    }

    fn tcp_listener(&self, sock: net::TcpListener) -> io::Result<crate::Listener> {
        Ok(super::accept::AcceptOps::get(self).listener(sock))
    }
}

impl ntex_rt::Driver for Reactor {
    /// Poll the driver and handle completed entries.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        let mut events = if self.capacity == 0 {
            Events::new()
        } else {
            Events::with_capacity(NonZeroUsize::new(self.capacity).unwrap())
        };

        let result = loop {
            let timeout = match rt.poll() {
                PollResult::Pending => None,
                PollResult::PollAgain => Some(Duration::ZERO),
                PollResult::Ready => break Ok(()),
            };
            // Runtime tasks can queue cleanup without receiving a stream event.
            let mut handlers = self.handlers.take().unwrap();
            self.tick(&mut handlers);
            self.handlers.set(Some(handlers));

            events.clear();
            self.poll.wait(&mut events, timeout)?;
            // tasks woken until the runtime is polled do not need to notify
            rt.awake();

            let mut handlers = self.handlers.take().unwrap();
            for (idx, event) in events.iter().enumerate() {
                if idx != 0 && idx % Self::EVENTS_PER_CHUNK == 0 {
                    // Run the tasks woken by the previous chunk within the
                    // same runtime turn. Handlers are released, so tasks
                    // can register new ones. A task can also release a
                    // registration with events still queued, handlers check
                    // the generation in their ids.
                    self.tick(&mut handlers);
                    self.handlers.set(Some(handlers));
                    rt.run_ready();
                    handlers = self.handlers.take().unwrap();
                }
                let key = event.key as u64;
                let batch = ((key & Self::BATCH_MASK) >> Self::BATCH) as usize;
                handlers[batch].event((key & Self::DATA_MASK) as usize, event);
            }
            self.tick(&mut handlers);
            self.handlers.set(Some(handlers));
        };

        for mut h in self.handlers.take().unwrap().into_iter() {
            h.cleanup();
        }
        result
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(NotifyHandle::new(self.poll.notifier()))
    }

    /// Clear handlers
    fn clear(&self) {}
}

#[derive(Clone, Debug)]
/// A notify handle to the inner driver.
pub(crate) struct NotifyHandle {
    notifier: Notifier,
}

impl NotifyHandle {
    fn new(notifier: Notifier) -> Self {
        Self { notifier }
    }
}

impl Notify for NotifyHandle {
    /// Notify the driver
    fn notify(&self) -> io::Result<()> {
        self.notifier.notify()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct Recorder(Rc<RefCell<Vec<(usize, io::ErrorKind)>>>);

    impl Handler for Recorder {
        fn event(&mut self, _: usize, _: Event) {}

        fn error(&mut self, id: usize, err: io::Error) {
            self.0.borrow_mut().push((id, err.kind()));
        }

        fn tick(&mut self) {}

        fn cleanup(&mut self) {}
    }

    #[test]
    fn reactor_info() {
        let reactor = Reactor::with_capacity(0).unwrap();
        assert_eq!(reactor.tp(), DriverType::Poll);
        assert!(reactor.as_raw_fd() >= 0);
        let s = format!("{reactor:?}");
        assert!(s.contains("Reactor") && s.contains("capacity"), "{s}");
    }

    /// Failed poller operations are reported to the handler that submitted them.
    #[test]
    fn api_errors_are_reported() {
        let reactor = Reactor::new().unwrap();
        let errors = Rc::new(RefCell::new(Vec::new()));
        let mut api = None;
        reactor.register(|a| {
            api = Some(a);
            Box::new(Recorder(errors.clone()))
        });
        let api = api.unwrap();

        // the socket is not attached
        let (sock, _peer) = OsUnixStream::pair().unwrap();
        api.modify(sock.as_raw_fd(), 1, Event::readable(0));
        api.detach(sock.as_raw_fd(), 2);
        assert!(errors.borrow().is_empty());

        let mut handlers = reactor.handlers.take().unwrap();
        reactor.apply_changes(&mut handlers);
        reactor.handlers.set(Some(handlers));

        let ids: Vec<_> = errors.borrow().iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn tick_drains_errors_and_their_deferred_work() {
        struct TickErrors {
            api: ReactorApi,
            socket: OsUnixStream,
            next: Option<u64>,
            errors: Rc<RefCell<Vec<usize>>>,
        }

        impl Handler for TickErrors {
            fn event(&mut self, _: usize, _: Event) {
                panic!("unexpected socket event");
            }

            fn error(&mut self, id: usize, _: io::Error) {
                self.errors.borrow_mut().push(id);
                if id == 1 {
                    self.next = Some(2);
                }
            }

            fn tick(&mut self) {
                if let Some(id) = self.next.take() {
                    // The socket is not attached, so the operation fails.
                    self.api
                        .modify(self.socket.as_raw_fd(), id, Event::readable(0));
                }
            }

            fn cleanup(&mut self) {}
        }

        let reactor = Reactor::new().unwrap();
        let (socket, _peer) = OsUnixStream::pair().unwrap();
        let errors = Rc::new(RefCell::new(Vec::new()));
        reactor.register(|api| {
            Box::new(TickErrors {
                api,
                socket,
                next: Some(1),
                errors: errors.clone(),
            })
        });

        let mut handlers = reactor.handlers.take().unwrap();
        reactor.tick(&mut handlers);
        reactor.handlers.set(Some(handlers));

        assert_eq!(*errors.borrow(), vec![1, 2]);
        // SAFETY: Dispatch has finished and no callback can mutate the thread-local queue.
        assert!(unsafe { (*reactor.changes.get()).is_empty() });
    }
}
