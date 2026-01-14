use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::{cell::Cell, cell::UnsafeCell, fmt, io, net, rc::Rc, sync::Arc};
use std::{collections::VecDeque, num::NonZeroUsize, time::Duration};

#[cfg(unix)]
use std::os::unix::net::UnixStream as OsUnixStream;

use ntex_io::Io;
use ntex_polling::{Event, Events, PollMode, Poller};
use ntex_rt::{DriverType, Notify, PollResult, Runtime};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{TcpStream, UnixStream, stream::StreamOps};
use crate::channel::Receiver;

pub trait Handler {
    /// Submitted interest
    fn event(&mut self, id: usize, event: Event);

    /// Operation submission has failed
    fn error(&mut self, id: usize, err: io::Error);

    /// Driver turn is completed
    fn tick(&mut self);

    /// Cleanup before drop
    fn cleanup(&mut self);
}

enum Change {
    Error {
        batch: usize,
        user_data: u32,
        error: io::Error,
    },
}

#[derive(Debug)]
pub struct DriverApi {
    id: usize,
    batch: u64,
    poll: Arc<Poller>,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
}

impl DriverApi {
    /// Attach an fd to the driver.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach(&self, fd: RawFd, id: u32, event: Event) {
        self.attach_with_mode(fd, id, event, PollMode::Oneshot)
    }

    /// Attach an fd to the driver with specific mode.
    ///
    /// `fd` must be attached to the driver before using register/unregister
    /// methods.
    pub fn attach_with_mode(&self, fd: RawFd, id: u32, mut event: Event, mode: PollMode) {
        event.key = (id as u64 | self.batch) as usize;
        if let Err(err) = unsafe { self.poll.add_with_mode(fd, event, mode) } {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Detach an fd from the driver.
    pub fn detach(&self, fd: RawFd, id: u32) {
        if let Err(err) = self.poll.delete(unsafe { BorrowedFd::borrow_raw(fd) }) {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    /// Register interest for specified file descriptor.
    pub fn modify(&self, fd: RawFd, id: u32, event: Event) {
        self.modify_with_mode(fd, id, event, PollMode::Oneshot)
    }

    /// Register interest for specified file descriptor.
    pub fn modify_with_mode(&self, fd: RawFd, id: u32, mut event: Event, mode: PollMode) {
        event.key = (id as u64 | self.batch) as usize;

        let result =
            self.poll
                .modify_with_mode(unsafe { BorrowedFd::borrow_raw(fd) }, event, mode);
        if let Err(err) = result {
            self.change(Change::Error {
                batch: self.id,
                user_data: id,
                error: err,
            })
        }
    }

    fn change(&self, ev: Change) {
        unsafe { (*self.changes.get()).push_back(ev) };
    }
}

/// Low-level driver of polling.
pub struct Driver {
    poll: Arc<Poller>,
    capacity: usize,
    changes: Rc<UnsafeCell<VecDeque<Change>>>,
    hid: Cell<u64>,
    #[allow(clippy::box_collection)]
    handlers: Cell<Option<Box<Vec<HandlerItem>>>>,
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

impl Driver {
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    pub fn new() -> io::Result<Self> {
        Driver::with_capacity(2048)
    }

    pub fn with_capacity(io_queue_capacity: u32) -> io::Result<Self> {
        log::trace!("New poll driver");

        Ok(Self {
            hid: Cell::new(0),
            poll: Arc::new(Poller::new()?),
            capacity: io_queue_capacity as usize,
            changes: Rc::new(UnsafeCell::new(VecDeque::with_capacity(32))),
            handlers: Cell::new(Some(Box::new(Vec::default()))),
        })
    }

    /// Driver type
    pub const fn tp(&self) -> DriverType {
        DriverType::Poll
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self
            .handlers
            .take()
            .expect("Cannot register handler during event handling");

        let api = DriverApi {
            id: id as usize,
            batch: id << Self::BATCH,
            poll: self.poll.clone(),
            changes: self.changes.clone(),
        };
        handlers.push(HandlerItem {
            hnd: f(api),
            modified: false,
        });
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    fn apply_changes(&self, handlers: &mut [HandlerItem]) {
        while let Some(op) = unsafe { (*self.changes.get()).pop_front() } {
            match op {
                Change::Error {
                    batch,
                    user_data,
                    error,
                } => handlers[batch].hnd.error(user_data as usize, error),
            }
        }
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("poll", &self.poll)
            .field("capacity", &self.capacity)
            .field("hid", &self.hid)
            .finish()
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
                super::connect::ConnectOps::get(self).connect(sock, addr, cfg, false)
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
                super::connect::ConnectOps::get(self).connect(sock, addr, cfg, true)
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
    /// Poll the driver and handle completed entries.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        let mut events = if self.capacity == 0 {
            Events::new()
        } else {
            Events::with_capacity(NonZeroUsize::new(self.capacity).unwrap())
        };

        loop {
            let result = rt.poll();
            let has_changes = !unsafe { (*self.changes.get()).is_empty() };
            if has_changes {
                let mut handlers = self.handlers.take().unwrap();
                self.apply_changes(&mut handlers);
                self.handlers.set(Some(handlers));
            }

            let timeout = match result {
                PollResult::Pending => None,
                PollResult::PollAgain => Some(Duration::ZERO),
                PollResult::Ready => return Ok(()),
            };
            events.clear();
            self.poll.wait(&mut events, timeout)?;

            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let key = event.key as u64;
                let batch = ((key & Self::BATCH_MASK) >> Self::BATCH) as usize;
                handlers[batch].modified = true;
                handlers[batch]
                    .hnd
                    .event((key & Self::DATA_MASK) as usize, event)
            }
            self.apply_changes(&mut handlers);
            for h in handlers.iter_mut() {
                h.tick();
            }
            self.handlers.set(Some(handlers));
        }
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(NotifyHandle::new(self.poll.clone()))
    }

    /// Clear handlers
    fn clear(&self) {
        for mut h in self.handlers.take().unwrap().into_iter() {
            h.hnd.cleanup()
        }
    }
}

#[derive(Clone, Debug)]
/// A notify handle to the inner driver.
pub(crate) struct NotifyHandle {
    poll: Arc<Poller>,
}

impl NotifyHandle {
    fn new(poll: Arc<Poller>) -> Self {
        Self { poll }
    }
}

impl Notify for NotifyHandle {
    /// Notify the driver
    fn notify(&self) -> io::Result<()> {
        self.poll.notify()
    }
}
