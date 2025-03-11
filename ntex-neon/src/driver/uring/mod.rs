pub use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use std::cell::{Cell, RefCell};
use std::{
    collections::VecDeque, io, pin::Pin, rc::Rc, sync::Arc, task::Poll, time::Duration,
};

use crossbeam_queue::SegQueue;
use io_uring::cqueue::{more, Entry as CEntry};
use io_uring::opcode::{AsyncCancel, PollAdd};
use io_uring::squeue::Entry as SEntry;
use io_uring::types::{Fd, SubmitArgs, Timespec};
use io_uring::IoUring;

mod notify;
pub(crate) mod op;

pub use self::notify::NotifyHandle;

use self::notify::Notifier;
use crate::driver::{sys, AsyncifyPool, Entry, Key, ProactorBuilder};

/// Abstraction of io-uring operations.
pub trait OpCode {
    /// Name of the operation
    fn name(&self) -> &'static str;

    /// Call the operation in a blocking way. This method will only be called if
    /// [`create_entry`] returns [`OpEntry::Blocking`].
    fn call_blocking(self: Pin<&mut Self>) -> io::Result<usize> {
        unreachable!("this operation is asynchronous")
    }

    /// Set the result when it successfully completes.
    /// The operation stores the result and is responsible to release it if the
    /// operation is cancelled.
    ///
    /// # Safety
    ///
    /// Users should not call it.
    unsafe fn set_result(self: Pin<&mut Self>, _: usize) {}
}

#[derive(Debug)]
enum Change {
    Submit { entry: SEntry },
    Cancel { op_id: u64 },
}

pub struct DriverApi {
    batch: u64,
    changes: Rc<RefCell<VecDeque<Change>>>,
}

impl DriverApi {
    pub fn submit(&self, user_data: u32, entry: SEntry) {
        log::debug!(
            "Submit operation batch: {:?} user-data: {:?} entry: {:?}",
            self.batch >> Driver::BATCH,
            user_data,
            entry,
        );
        self.changes.borrow_mut().push_back(Change::Submit {
            entry: entry.user_data(user_data as u64 | self.batch),
        });
    }

    pub fn cancel(&self, op_id: u32) {
        log::debug!(
            "Cancel operation batch: {:?} user-data: {:?}",
            self.batch >> Driver::BATCH,
            op_id
        );
        self.changes.borrow_mut().push_back(Change::Cancel {
            op_id: op_id as u64 | self.batch,
        });
    }
}

/// Low-level driver of io-uring.
pub(crate) struct Driver {
    ring: RefCell<IoUring<SEntry, CEntry>>,
    notifier: Notifier,
    pool: AsyncifyPool,
    pool_completed: Arc<SegQueue<Entry>>,

    hid: Cell<u64>,
    changes: Rc<RefCell<VecDeque<Change>>>,
    handlers: Cell<Option<Box<Vec<Box<dyn op::Handler>>>>>,
}

impl Driver {
    const NOTIFY: u64 = u64::MAX;
    const CANCEL: u64 = u64::MAX - 1;
    const BATCH: u64 = 48;
    const BATCH_MASK: u64 = 0xFFFF_0000_0000_0000;
    const DATA_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

    pub fn new(builder: &ProactorBuilder) -> io::Result<Self> {
        log::trace!("New io-uring driver");

        let mut ring = IoUring::builder()
            .setup_coop_taskrun()
            .setup_single_issuer()
            .build(builder.capacity)?;

        let notifier = Notifier::new()?;

        #[allow(clippy::useless_conversion)]
        unsafe {
            ring.submission()
                .push(
                    &PollAdd::new(Fd(notifier.as_raw_fd()), libc::POLLIN as _)
                        .multi(true)
                        .build()
                        .user_data(Self::NOTIFY)
                        .into(),
                )
                .expect("the squeue sould not be full");
        }
        Ok(Self {
            notifier,
            ring: RefCell::new(ring),
            pool: builder.create_or_get_thread_pool(),
            pool_completed: Arc::new(SegQueue::new()),

            hid: Cell::new(0),
            changes: Rc::new(RefCell::new(VecDeque::new())),
            handlers: Cell::new(Some(Box::new(Vec::new()))),
        })
    }

    pub fn register_handler<F>(&self, f: F)
    where
        F: FnOnce(DriverApi) -> Box<dyn self::op::Handler>,
    {
        let id = self.hid.get();
        let mut handlers = self.handlers.take().unwrap_or_default();

        let api = DriverApi {
            batch: id << 48,
            changes: self.changes.clone(),
        };
        handlers.push(f(api));
        self.hid.set(id + 1);
        self.handlers.set(Some(handlers));
    }

    // Auto means that it choose to wait or not automatically.
    fn submit_auto(&self, timeout: Option<Duration>) -> io::Result<()> {
        let mut ring = self.ring.borrow_mut();
        let res = {
            // Last part of submission queue, wait till timeout.
            if let Some(duration) = timeout {
                let timespec = timespec(duration);
                let args = SubmitArgs::new().timespec(&timespec);
                ring.submitter().submit_with_args(1, &args)
            } else {
                ring.submit_and_wait(1)
            }
        };
        match res {
            Ok(_) => {
                // log::debug!("Submit result: {res:?} {:?}", timeout);
                if ring.completion().is_empty() {
                    Err(io::ErrorKind::TimedOut.into())
                } else {
                    Ok(())
                }
            }
            Err(e) => match e.raw_os_error() {
                Some(libc::ETIME) => {
                    if timeout.is_some() && timeout != Some(Duration::ZERO) {
                        Err(io::ErrorKind::TimedOut.into())
                    } else {
                        Ok(())
                    }
                }
                Some(libc::EBUSY) | Some(libc::EAGAIN) => {
                    Err(io::ErrorKind::Interrupted.into())
                }
                _ => Err(e),
            },
        }
    }

    pub fn create_op<T: sys::OpCode + 'static>(&self, op: T) -> Key<T> {
        Key::new(self.as_raw_fd(), op)
    }

    fn apply_changes(&self) -> bool {
        let mut changes = self.changes.borrow_mut();
        if changes.is_empty() {
            return false;
        }
        log::debug!("Apply changes, {:?}", changes.len());

        let mut ring = self.ring.borrow_mut();
        let mut squeue = ring.submission();

        while let Some(change) = changes.pop_front() {
            match change {
                Change::Submit { entry } => {
                    if unsafe { squeue.push(&entry) }.is_err() {
                        changes.push_front(Change::Submit { entry });
                        break;
                    }
                }
                Change::Cancel { op_id } => {
                    let entry = AsyncCancel::new(op_id).build().user_data(Self::CANCEL);
                    if unsafe { squeue.push(&entry) }.is_err() {
                        changes.push_front(Change::Cancel { op_id });
                        break;
                    }
                }
            }
        }
        squeue.sync();

        !changes.is_empty()
    }

    pub unsafe fn poll<F: FnOnce()>(
        &self,
        timeout: Option<Duration>,
        f: F,
    ) -> io::Result<()> {
        self.poll_blocking();

        let has_more = self.apply_changes();
        let poll_result = self.poll_completions();

        if !poll_result || has_more {
            if has_more {
                self.submit_auto(Some(Duration::ZERO))?;
            } else {
                self.submit_auto(timeout)?;
            }
            self.poll_completions();
        }

        f();

        Ok(())
    }

    pub fn push(&self, op: &mut Key<dyn sys::OpCode>) -> Poll<io::Result<usize>> {
        log::trace!("Push op: {:?}", op.as_op_pin().name());

        let user_data = op.user_data();
        loop {
            if self.push_blocking(user_data) {
                break Poll::Pending;
            } else {
                self.poll_blocking();
            }
        }
    }

    fn poll_completions(&self) -> bool {
        let mut ring = self.ring.borrow_mut();
        let mut cqueue = ring.completion();
        cqueue.sync();
        let has_entry = !cqueue.is_empty();
        if !has_entry {
            return false;
        }
        let mut handlers = self.handlers.take().unwrap();
        for entry in cqueue {
            let user_data = entry.user_data();
            match user_data {
                Self::CANCEL => {}
                Self::NOTIFY => {
                    let flags = entry.flags();
                    debug_assert!(more(flags));
                    self.notifier.clear().expect("cannot clear notifier");
                }
                _ => {
                    let batch = ((user_data & Self::BATCH_MASK) >> Self::BATCH) as usize;
                    let user_data = (user_data & Self::DATA_MASK) as usize;

                    let result = entry.result();

                    if result == -libc::ECANCELED {
                        handlers[batch].canceled(user_data);
                    } else {
                        let result = if result < 0 {
                            Err(io::Error::from_raw_os_error(result))
                        } else {
                            Ok(result as _)
                        };
                        handlers[batch].completed(user_data, entry.flags(), result);
                    }
                }
            }
        }
        self.handlers.set(Some(handlers));
        true
    }

    fn poll_blocking(&self) {
        if !self.pool_completed.is_empty() {
            while let Some(entry) = self.pool_completed.pop() {
                unsafe {
                    entry.notify();
                }
            }
        }
    }

    fn push_blocking(&self, user_data: usize) -> bool {
        let handle = self.handle();
        let completed = self.pool_completed.clone();
        self.pool
            .dispatch(move || {
                let mut op = unsafe { Key::<dyn sys::OpCode>::new_unchecked(user_data) };
                let op_pin = op.as_op_pin();
                let res = op_pin.call_blocking();
                completed.push(Entry::new(user_data, res));
                handle.notify().ok();
            })
            .is_ok()
    }

    pub fn handle(&self) -> NotifyHandle {
        self.notifier.handle()
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.ring.borrow().as_raw_fd()
    }
}

fn timespec(duration: std::time::Duration) -> Timespec {
    Timespec::new()
        .sec(duration.as_secs())
        .nsec(duration.subsec_nanos())
}
