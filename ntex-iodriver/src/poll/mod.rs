#![allow(clippy::type_complexity)]
pub use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use std::{cell::Cell, cell::RefCell, collections::HashMap, io, rc::Rc, sync::Arc};
use std::{num::NonZeroUsize, os::fd::BorrowedFd, pin::Pin, task::Poll, time::Duration};

use crossbeam_queue::SegQueue;
use polling::{Event, Events, Poller};

use crate::{
    op::Handler, op::Interest, syscall, AsyncifyPool, Entry, Key, ProactorBuilder,
};

pub(crate) mod op;

/// Abstraction of operations.
pub trait OpCode {
    /// Perform the operation before submit, and return [`Decision`] to
    /// indicate whether submitting the operation to polling is required.
    fn pre_submit(self: Pin<&mut Self>) -> io::Result<Decision>;

    /// Perform the operation after received corresponding
    /// event. If this operation is blocking, the return value should be
    /// [`Poll::Ready`].
    fn operate(self: Pin<&mut Self>) -> Poll<io::Result<usize>>;
}

/// Result of [`OpCode::pre_submit`].
#[non_exhaustive]
pub enum Decision {
    /// Instant operation, no need to submit
    Completed(usize),
    /// Blocking operation, needs to be spawned in another thread
    Blocking,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const NEW     = 0b0000_0001;
        const CHANGED = 0b0000_0010;
    }
}

#[derive(Debug)]
struct FdItem {
    flags: Flags,
    batch: usize,
    read: Option<usize>,
    write: Option<usize>,
}

impl FdItem {
    fn new(batch: usize) -> Self {
        Self {
            batch,
            read: None,
            write: None,
            flags: Flags::NEW,
        }
    }

    fn register(&mut self, user_data: usize, interest: Interest) {
        self.flags.insert(Flags::CHANGED);
        match interest {
            Interest::Readable => {
                self.read = Some(user_data);
            }
            Interest::Writable => {
                self.write = Some(user_data);
            }
        }
    }

    fn unregister(&mut self, int: Interest) {
        let res = match int {
            Interest::Readable => self.read.take(),
            Interest::Writable => self.write.take(),
        };
        if res.is_some() {
            self.flags.insert(Flags::CHANGED);
        }
    }

    fn unregister_all(&mut self) {
        if self.read.is_some() || self.write.is_some() {
            self.flags.insert(Flags::CHANGED);
        }

        let _ = self.read.take();
        let _ = self.write.take();
    }

    fn user_data(&mut self, interest: Interest) -> Option<usize> {
        match interest {
            Interest::Readable => self.read,
            Interest::Writable => self.write,
        }
    }

    fn event(&self, key: usize) -> Event {
        let mut event = Event::none(key);
        if self.read.is_some() {
            event.readable = true;
        }
        if self.write.is_some() {
            event.writable = true;
        }
        event
    }
}

#[derive(Debug)]
enum Change {
    Register {
        fd: RawFd,
        batch: usize,
        user_data: usize,
        int: Interest,
    },
    Unregister {
        fd: RawFd,
        batch: usize,
        int: Interest,
    },
    UnregisterAll {
        fd: RawFd,
        batch: usize,
    },
    Blocking {
        user_data: usize,
    },
}

// #[derive(Debug)]
// struct BatchChange {
//     fd: RawFd,
//     batch: usize,
//     user_data: usize,
//     interest: InterestChange,
// }

pub struct DriverApi {
    batch: usize,
    changes: Rc<RefCell<Vec<Change>>>,
}

impl DriverApi {
    pub fn register(&self, fd: RawFd, user_data: usize, int: Interest) {
        log::debug!(
            "Register interest {:?} for {:?} user-data: {:?}",
            int,
            fd,
            user_data
        );
        self.change(Change::Register {
            fd,
            batch: self.batch,
            user_data,
            int,
        });
    }

    pub fn unregister(&self, fd: RawFd, int: Interest) {
        log::debug!(
            "Unregister interest {:?} for {:?} batch: {:?}",
            int,
            fd,
            self.batch
        );
        self.change(Change::Unregister {
            fd,
            batch: self.batch,
            int,
        });
    }

    pub fn unregister_all(&self, fd: RawFd) {
        self.change(Change::UnregisterAll {
            fd,
            batch: self.batch,
        });
    }

    fn change(&self, change: Change) {
        self.changes.borrow_mut().push(change);
    }
}

/// Low-level driver of polling.
pub(crate) struct Driver {
    poll: Arc<Poller>,
    events: RefCell<Events>,
    registry: RefCell<HashMap<RawFd, FdItem>>,
    pool: AsyncifyPool,
    pool_completed: Arc<SegQueue<Entry>>,
    hid: Cell<usize>,
    changes: Rc<RefCell<Vec<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
    pub fn new(builder: &ProactorBuilder) -> io::Result<Self> {
        log::trace!("New poll driver");
        let entries = builder.capacity as usize; // for the sake of consistency, use u32 like iour
        let events = if entries == 0 {
            Events::new()
        } else {
            Events::with_capacity(NonZeroUsize::new(entries).unwrap())
        };

        Ok(Self {
            poll: Arc::new(Poller::new()?),
            events: RefCell::new(events),
            registry: RefCell::new(HashMap::default()),
            pool: builder.create_or_get_thread_pool(),
            pool_completed: Arc::new(SegQueue::new()),
            hid: Cell::new(0),
            changes: Rc::new(RefCell::new(Vec::with_capacity(16))),
            handlers: Cell::new(Some(Box::new(Vec::default()))),
        })
    }

    pub fn register_handler<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            batch: id,
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    pub fn create_op<T: crate::sys::OpCode + 'static>(&self, op: T) -> Key<T> {
        Key::new(self.as_raw_fd(), op)
    }

    fn renew(
        &self,
        fd: BorrowedFd,
        renew_event: Event,
        registry: &mut HashMap<RawFd, FdItem>,
    ) -> io::Result<()> {
        if !renew_event.readable && !renew_event.writable {
            // crate::log(format!("DELETE - {:?}", fd.as_raw_fd()));

            if let Some(item) = registry.remove(&fd.as_raw_fd()) {
                if !item.flags.contains(Flags::NEW) {
                    self.poll.delete(fd)?;
                }
            }
        } else {
            if let Some(item) = registry.get(&fd.as_raw_fd()) {
                if item.flags.contains(Flags::NEW) {
                    // crate::log(format!("ADD - {:?}", fd.as_raw_fd()));
                    unsafe { self.poll.add(&fd, renew_event)? };
                    return Ok(());
                }
            }
            // crate::log(format!("MODIFY - {:?} {:?}", fd.as_raw_fd(), renew_event));
            self.poll.modify(fd, renew_event)?;
        }
        Ok(())
    }

    pub fn attach(&self, _fd: RawFd) -> io::Result<()> {
        Ok(())
    }

    pub fn push(&self, op: &mut Key<dyn crate::sys::OpCode>) -> Poll<io::Result<usize>> {
        let user_data = op.user_data();
        let op_pin = op.as_op_pin();
        match op_pin.pre_submit()? {
            Decision::Completed(res) => Poll::Ready(Ok(res)),
            Decision::Blocking => {
                self.changes
                    .borrow_mut()
                    .push(Change::Blocking { user_data });
                Poll::Pending
            }
        }
    }

    pub unsafe fn poll<F: FnOnce()>(
        &self,
        timeout: Option<Duration>,
        f: F,
    ) -> io::Result<()> {
        if self.poll_blocking() {
            f();
            self.apply_changes()?;
            return Ok(());
        }

        let mut events = self.events.borrow_mut();
        let res = self.poll.wait(&mut events, timeout);
        res?;

        if events.is_empty() && timeout != Some(Duration::ZERO) && timeout.is_some() {
            return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
        }
        // println!("POLL, events: {:?}", events.len());

        if !events.is_empty() {
            let mut registry = self.registry.borrow_mut();
            let mut handlers = self.handlers.take().unwrap();
            for event in events.iter() {
                let user_data = event.key;
                let fd = user_data as RawFd;
                log::debug!(
                    "receive {} for {:?} {:?}",
                    user_data,
                    event,
                    registry.get_mut(&fd)
                );

                if let Some(item) = registry.get_mut(&fd) {
                    self.handle_batch_event(event, item, &mut handlers);
                }
            }
            drop(registry);
            self.handlers.set(Some(handlers));
        }

        // apply changes
        self.apply_changes()?;

        // complete batch handling
        let mut handlers = self.handlers.take().unwrap();
        for handler in handlers.iter_mut() {
            handler.commit();
        }
        self.handlers.set(Some(handlers));
        self.apply_changes()?;

        // run user function
        f();

        // check if we have more changes from "run"
        self.apply_changes()?;

        Ok(())
    }

    fn handle_batch_event(
        &self,
        event: Event,
        item: &mut FdItem,
        handlers: &mut [Box<dyn Handler>],
    ) {
        if event.readable {
            if let Some(user_data) = item.user_data(Interest::Readable) {
                handlers[item.batch].readable(user_data)
            }
        }
        if event.writable {
            if let Some(user_data) = item.user_data(Interest::Writable) {
                handlers[item.batch].writable(user_data)
            }
        }
    }

    /// re-calc driver changes
    unsafe fn apply_changes(&self) -> io::Result<()> {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return Ok(());
        }

        log::debug!("Apply driver changes, {:?}", changes.len());

        let mut registry = self.registry.borrow_mut();

        for change in &mut *changes {
            match change {
                Change::Register {
                    fd,
                    batch,
                    user_data,
                    int,
                } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.register(*user_data, *int);
                }
                Change::Unregister { fd, batch, int } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.unregister(*int);
                }
                Change::UnregisterAll { fd, batch } => {
                    let item = registry.entry(*fd).or_insert_with(|| FdItem::new(*batch));
                    item.unregister_all();
                }
                _ => {}
            }
        }

        for change in changes.drain(..) {
            let fd = match change {
                Change::Register { fd, .. } => Some(fd),
                Change::Unregister { fd, .. } => Some(fd),
                Change::UnregisterAll { fd, .. } => Some(fd),
                Change::Blocking { user_data } => {
                    self.push_blocking(user_data);
                    None
                }
            };

            if let Some(fd) = fd {
                let result = registry.get_mut(&fd).and_then(|item| {
                    if item.flags.contains(Flags::CHANGED) {
                        item.flags.remove(Flags::CHANGED);
                        Some((item.event(fd as usize), item.flags.contains(Flags::NEW)))
                    } else {
                        None
                    }
                });
                if let Some((event, new)) = result {
                    self.renew(BorrowedFd::borrow_raw(fd), event, &mut registry)?;

                    if new {
                        if let Some(item) = registry.get_mut(&fd) {
                            item.flags.remove(Flags::NEW);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn push_blocking(&self, user_data: usize) {
        // -> Poll<io::Result<usize>> {
        let poll = self.poll.clone();
        let completed = self.pool_completed.clone();
        let mut closure = move || {
            let mut op = unsafe { Key::<dyn crate::sys::OpCode>::new_unchecked(user_data) };
            let op_pin = op.as_op_pin();
            let res = match op_pin.operate() {
                Poll::Pending => unreachable!("this operation is not non-blocking"),
                Poll::Ready(res) => res,
            };
            completed.push(Entry::new(user_data, res));
            poll.notify().ok();
        };
        loop {
            match self.pool.dispatch(closure) {
                Ok(()) => return,
                Err(e) => {
                    closure = e.0;
                    self.poll_blocking();
                }
            }
        }
    }

    fn poll_blocking(&self) -> bool {
        if self.pool_completed.is_empty() {
            return false;
        }

        while let Some(entry) = self.pool_completed.pop() {
            unsafe {
                entry.notify();
            }
        }
        true
    }

    pub fn handle(&self) -> NotifyHandle {
        NotifyHandle::new(self.poll.clone())
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        for fd in self.registry.borrow().keys() {
            unsafe {
                let fd = BorrowedFd::borrow_raw(*fd);
                self.poll.delete(fd).ok();
            }
        }
    }
}

#[derive(Clone)]
/// A notify handle to the inner driver.
pub struct NotifyHandle {
    poll: Arc<Poller>,
}

impl NotifyHandle {
    fn new(poll: Arc<Poller>) -> Self {
        Self { poll }
    }

    /// Notify the inner driver.
    pub fn notify(&self) -> io::Result<()> {
        self.poll.notify()
    }
}
