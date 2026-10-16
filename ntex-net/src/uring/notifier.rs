use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::{io, sync::Arc};

use ntex_io_uring::opcode::Read;
use ntex_io_uring::squeue::{Entry as SEntry, SubmissionQueue};
use ntex_io_uring::{IoUring, types::CancelBuilder, types::Fd, types::Timespec};
use ntex_rt::{Notify, syscall};

pub(super) const NOTIFY: u64 = u64::MAX;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum NotifierState {
    // Arming requested, but no SQE references the buffer yet.
    Queued,
    // The read is in the SQ, in flight, or awaiting completion processing.
    Active,
    Stopped,
    // Teardown could not prove that the kernel is done with the buffer.
    Retained,
}

pub(super) struct NotifierRead {
    pub(super) buf: Option<Box<UnsafeCell<u64>>>,
    pub(super) state: Cell<NotifierState>,
}

impl NotifierRead {
    pub(super) fn new() -> Self {
        Self {
            buf: Some(Box::new(UnsafeCell::new(0))),
            state: Cell::new(NotifierState::Queued),
        }
    }

    pub(super) fn guard<'a>(&'a self, ring: &'a IoUring) -> NotifierGuard<'a> {
        NotifierGuard { read: self, ring }
    }

    pub(super) fn is_queued(&self) -> bool {
        self.state.get() == NotifierState::Queued
    }

    pub(super) fn arm(&self, sq: SubmissionQueue<'_, SEntry>, fd: RawFd) {
        if self.is_queued() {
            // Do not put notifier reads in the overflow queue. Until
            // this succeeds, teardown can discard the arming request.
            let _ = unsafe {
                sq.push(|en| {
                    self.state.set(NotifierState::Active);
                    Read::new(en, Fd(fd), self.buf.as_ref().unwrap().get().cast(), 8)
                        .user_data(NOTIFY);
                })
            };
        }
    }

    pub(super) fn completed(&self, res: i32) {
        // the read resets the eventfd counter
        self.state
            .set(if self.state.get() == NotifierState::Active {
                NotifierState::Queued
            } else {
                NotifierState::Stopped
            });
        assert!(
            res >= 0 || res == -libc::EINTR || res == -libc::ECANCELED,
            "cannot read notifier: {}",
            io::Error::from_raw_os_error(-res)
        );
    }

    pub(super) fn stop_arming(&self) {
        if self.is_queued() {
            self.state.set(NotifierState::Stopped);
        }
    }

    pub(super) fn stop(&self, ring: &IoUring) {
        self.stop_with(ring, || {
            ring.submitter().register_sync_cancel(
                Some(Timespec::new().sec(1)),
                CancelBuilder::user_data(NOTIFY),
            )
        });
    }

    pub(super) fn stop_with(&self, ring: &IoUring, cancel: impl FnOnce() -> io::Result<()>) {
        match self.state.get() {
            NotifierState::Queued => self.state.set(NotifierState::Stopped),
            NotifierState::Active => {
                self.state.set(NotifierState::Retained);
                let result = cancel();
                self.canceled(ring, &result);
                if let Err(e) = result
                    && e.raw_os_error() != Some(libc::ENOENT)
                {
                    log::error!("Cannot cancel io-uring notifier read: {e:?}");
                }
            }
            NotifierState::Stopped | NotifierState::Retained => {}
        }
    }

    pub(super) fn canceled(&self, ring: &IoUring, result: &io::Result<()>) {
        // Cancellation does not cover SQEs the kernel has not consumed.
        // In particular, ENOENT alone cannot establish buffer safety.
        if (result.is_ok()
            || result
                .as_ref()
                .is_err_and(|e| e.raw_os_error() == Some(libc::ENOENT)))
            && ring.submission().is_empty()
        {
            self.state.set(NotifierState::Stopped);
        }
    }
}

impl Drop for NotifierRead {
    fn drop(&mut self) {
        if matches!(
            self.state.get(),
            NotifierState::Active | NotifierState::Retained
        ) {
            // Closing the ring is not a synchronous cancellation barrier.
            Box::leak(self.buf.take().unwrap());
            log::error!("Retaining io-uring notifier buffer: read may still reference it");
        }
    }
}

pub(super) struct NotifierGuard<'a> {
    read: &'a NotifierRead,
    ring: &'a IoUring,
}

impl Drop for NotifierGuard<'_> {
    fn drop(&mut self) {
        self.read.stop(self.ring);
    }
}

#[derive(Debug)]
pub(super) struct Notifier {
    fd: Arc<OwnedFd>,
}

impl Notifier {
    /// Create a new notifier.
    pub(super) fn new() -> io::Result<Self> {
        // io-uring fails reads of non-blocking files with EAGAIN instead of
        // waiting for data, a write blocks only on the counter overflow
        let fd = syscall!(libc::eventfd(0, libc::EFD_CLOEXEC))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Self { fd: Arc::new(fd) })
    }

    pub(super) fn handle(&self) -> NotifyHandle {
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
pub(super) struct NotifyHandle {
    fd: Arc<OwnedFd>,
}

impl NotifyHandle {
    pub(super) fn new(fd: Arc<OwnedFd>) -> Self {
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
