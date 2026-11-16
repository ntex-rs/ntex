#![allow(clippy::cast_possible_wrap)]
use std::{cmp, io, mem, os::windows::io::RawSocket, ptr, task::Poll};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
        ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError,
    },
    Networking::WinSock::{SOCKET_ERROR, WSABUF, WSAEWOULDBLOCK, WSARecv, WSASend, recv},
    System::IO::{CancelIoEx, OVERLAPPED},
};

use ntex_bytes::{BufMut, BytePages};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::syscall;

use super::{Op, Overlapped};

pub(crate) const RD_OP: u32 = 1;
pub(crate) const WR_OP: u32 = 2;
const MAX_WRITE_BUFS: usize = 32;
/// Bytes sent at once, the kernel locks the pages of a send in memory.
const MAX_WRITE_SIZE: usize = 256 * 1024;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const WAITING     = 0b0000_0001;
        const CLOSING     = 0b0000_0010;
        /// The socket could not be switched to non-blocking mode, so it is
        /// only read once the kernel reported it readable.
        const BLOCKING    = 0b0000_0100;
        /// Reading is paused, by the io layer or by the read status, a recv
        /// that completes or a notification that arrives meanwhile is not
        /// followed by a read.
        const PAUSED      = 0b0000_1000;
        /// Input is reported by socket state notifications instead of a
        /// zero-byte recv.
        const NOTIFY      = 0b0001_0000;
    }
}

#[derive(Debug)]
pub(crate) struct ReadOperation {
    id: usize, // idx for StreamItem
    io: RawSocket,
    ctx: IoContext,
    flags: Flags,
}

impl ReadOperation {
    /// Creates the read operation of a socket.
    ///
    /// `nonblocking` tells whether the socket is in non-blocking mode, which
    /// lets it be read without waiting for the kernel to report it readable.
    pub(crate) fn new(id: usize, io: RawSocket, ctx: IoContext, nonblocking: bool) -> Self {
        Self {
            id,
            io,
            ctx,
            flags: if nonblocking {
                Flags::empty()
            } else {
                Flags::BLOCKING
            },
        }
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.ctx.tag()
    }

    /// The connection's shutdown timeout, it bounds the wait for cancelled
    /// operations too.
    pub(crate) fn shutdown_timeout(&self) -> ntex_util::time::Seconds {
        self.ctx.shutdown_timeout()
    }

    /// Marks a recv as in flight without issuing one, so that the kernel never
    /// completes it, as when a cancel does not take.
    #[cfg(test)]
    pub(crate) fn fake_pending(&mut self) {
        self.flags.insert(Flags::WAITING);
    }

    /// Reads as if the socket could not be switched to non-blocking mode.
    #[cfg(test)]
    pub(crate) fn fake_blocking(&mut self) {
        self.flags.insert(Flags::BLOCKING);
    }

    /// Whether a recv is in flight, so the kernel still owns this operation.
    pub(crate) fn is_pending(&self) -> bool {
        self.flags.contains(Flags::WAITING)
    }

    /// Whether the socket could not be switched to non-blocking mode.
    pub(crate) fn is_blocking(&self) -> bool {
        self.flags.contains(Flags::BLOCKING)
    }

    /// Whether input is reported by socket state notifications.
    #[cfg(test)]
    pub(crate) fn is_notified(&self) -> bool {
        self.flags.contains(Flags::NOTIFY)
    }

    /// Waits for input with socket state notifications instead of a zero-byte
    /// recv, the socket is registered for them.
    pub(crate) fn use_notifications(&mut self) {
        self.flags.insert(Flags::NOTIFY);
    }

    /// Input is available, reads it unless reading is paused or closing.
    pub(crate) fn notified(&mut self, overlapped: *mut OVERLAPPED) {
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: RcvNotify({}) {:?}",
            self.ctx.tag(),
            self.io,
            self.flags
        );
        if !self.flags.intersects(Flags::PAUSED | Flags::CLOSING) {
            self.read(overlapped);
        }
    }

    /// Pauses reading until the next `read()`.
    ///
    /// A recv in flight is left running: it is zero-byte, so it holds no
    /// buffer, and cancelling it would cost a syscall and a completion on every
    /// pause. If it completes during the pause, the input stays in the socket
    /// until reading resumes.
    pub(crate) fn pause(&mut self) {
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: RcvPause({}) flags:{:?}",
            self.ctx.tag(),
            self.io,
            self.flags
        );
        self.flags.insert(Flags::PAUSED);
    }

    /// Cancels the recv in flight for a close, its completion starts the close.
    /// Returns `true` if none is in flight.
    pub(crate) fn cancel(&mut self, overlapped: *mut OVERLAPPED) -> bool {
        if self.flags.contains(Flags::WAITING) {
            // a null pointer would cancel every operation of the socket
            debug_assert!(!overlapped.is_null(), "recv without a read header");
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: RcvCancel({}) flags:{:?}",
                self.ctx.tag(),
                self.io,
                self.flags
            );

            if let Err(err) = syscall!(BOOL, CancelIoEx(self.io as _, overlapped)) {
                let e = err.raw_os_error();
                if e != Some(ERROR_NOT_FOUND as _) && e != Some(ERROR_OPERATION_ABORTED as _) {
                    // The recv is still live and the kernel still owns the
                    // operation, so fall through and wait for the completion
                    // rather than reporting the op as finished.
                    log::error!(
                        "{}: failed to cancel recv({}): {err:?}",
                        self.ctx.tag(),
                        self.io
                    );
                }
            }
            self.flags.insert(Flags::CLOSING);
            false
        } else {
            true
        }
    }

    /// Reads available input into the read buffer of the connection.
    ///
    /// Input is read in place with a non-blocking `recv`. Once the socket has
    /// no more input, a zero-byte overlapped recv is posted to wait for more,
    /// so no buffer is handed to the kernel while the connection is idle, and
    /// input that arrives while earlier input is still unconsumed is appended
    /// to it instead of going into a buffer of its own.
    ///
    /// `overlapped` is the read op's kernel header, null for a socket read
    /// through notifications, which never waits with a recv.
    pub(crate) fn read(&mut self, overlapped: *mut OVERLAPPED) {
        self.flags.remove(Flags::PAUSED);
        if self.flags.contains(Flags::WAITING) {
            return;
        }

        #[cfg(feature = "trace")]
        log::trace!("{}: Rcv({})", self.ctx.tag(), self.io);

        // A blocking socket is only read once it has been reported readable
        let mut readable = !self.flags.contains(Flags::BLOCKING);
        loop {
            if readable {
                // a read that fills the buffer is likely to leave more input
                // behind, a shorter one most likely drained the socket
                let mut drained = false;
                let st = self.ctx.with_read_buf(|buf| {
                    let chunk = buf.chunk_mut();
                    let len = i32::try_from(chunk.len()).unwrap_or(i32::MAX);
                    // SAFETY: The live socket writes at most `len` bytes into this borrowed spare chunk.
                    let res = unsafe { recv(self.io as _, chunk.as_mut_ptr(), len, 0) };
                    if res == SOCKET_ERROR {
                        let err = io::Error::last_os_error();
                        if err.raw_os_error() == Some(WSAEWOULDBLOCK) {
                            drained = true;
                            Poll::Pending
                        } else {
                            Poll::Ready(Err(err))
                        }
                    } else {
                        let size = usize::try_from(res).unwrap_or_default();
                        // a queued FIN raises no edge after a short read, so a
                        // notified socket is read until it would block
                        drained = res < len && !self.flags.contains(Flags::NOTIFY);
                        // SAFETY: winsock tells us how many bytes it read
                        unsafe { buf.advance_mut(size) };
                        Poll::Ready(Ok(size))
                    }
                });
                if st != IoTaskStatus::Io {
                    // Reading resumes only through `read()`, once the io
                    // layer reports read readiness again. A notification
                    // meanwhile must not read past the back-pressure.
                    self.flags.insert(Flags::PAUSED);
                    break;
                }
                if !drained && !self.flags.contains(Flags::BLOCKING) {
                    continue;
                }
            }

            // a notification reports more input, the socket is drained
            if self.flags.contains(Flags::NOTIFY) {
                break;
            }

            // wait for more input without handing a buffer to the kernel
            debug_assert!(!overlapped.is_null(), "recv without a read header");
            let lpbufs = [WSABUF {
                len: 0,
                buf: ptr::null_mut(),
            }];
            let mut size = 0;
            let mut flags = 0;
            // SAFETY: Winsock copies the zero-length descriptor; the driver retains the header
            // until completion, and the stack outputs are used only during this call.
            let result = unsafe {
                WSARecv(
                    self.io as _,
                    ptr::from_ref(&lpbufs[0]),
                    1,
                    &raw mut size,
                    &raw mut flags,
                    overlapped,
                    None,
                )
            };

            match winsock_result(result) {
                // input is available already, completions are skipped on success
                Poll::Ready(Ok(())) => readable = true,
                Poll::Ready(Err(err)) => {
                    self.ctx.with_read_buf(|_| Poll::Ready(Err(err)));
                    break;
                }
                Poll::Pending => {
                    self.flags.insert(Flags::WAITING);
                    break;
                }
            }
        }
    }

    pub(crate) fn completed(res: io::Result<usize>, optr: *mut Overlapped) -> Option<usize> {
        // SAFETY: RD_OP identifies a live read op; only its payload is borrowed
        let rd = unsafe { &mut *Op::<Self>::data_from_overlapped(optr) };

        #[cfg(feature = "trace")]
        log::trace!(
            "{}: RcvDone({}) {res:?} {:?}",
            rd.ctx.tag(),
            rd.io,
            rd.flags
        );

        rd.flags.remove(Flags::WAITING);

        // The recv is zero-byte, it only reports that input is available. The
        // input stays in the socket, and a close discards it.
        if rd.flags.contains(Flags::CLOSING) {
            return Some(rd.id);
        }
        match res {
            Err(err) if err.raw_os_error() != Some(ERROR_OPERATION_ABORTED as _) => {
                rd.ctx.with_read_buf(|_| Poll::Ready(Err(err)));
            }
            // input is available, or the recv was cancelled. A paused read
            // resumes through `read()`, which then finds no recv in flight.
            _ => {
                if !rd.flags.contains(Flags::PAUSED) {
                    rd.read(optr.cast());
                }
            }
        }
        None
    }
}

#[derive(Debug)]
pub(crate) struct WriteOperation {
    id: usize, // idx for StreamItem
    io: RawSocket,
    ctx: IoContext,
    flags: Flags,
    /// Output taken from the write buffer and not sent yet. The kernel reads
    /// from these pages while a send is in flight, so they are not modified
    /// until it completes.
    pages: BytePages,
}

impl WriteOperation {
    pub(crate) fn new(id: usize, io: RawSocket, ctx: IoContext) -> Self {
        Self {
            id,
            io,
            ctx,
            flags: Flags::empty(),
            pages: BytePages::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn fake_pending(&mut self) {
        self.flags.insert(Flags::WAITING);
    }

    /// Marks a send of `pages` as in flight, without sending it.
    #[cfg(test)]
    pub(crate) fn fake_pending_with(&mut self, pages: &[&'static [u8]]) {
        for page in pages.iter().rev() {
            self.pages.prepend(ntex_bytes::Bytes::from_static(page));
        }
        self.flags.insert(Flags::WAITING);
    }

    /// Number of bytes taken from the write buffer and not sent yet.
    #[cfg(test)]
    pub(crate) fn unsent(&self) -> usize {
        self.pages.len()
    }

    /// Whether the send was cancelled for a close.
    #[cfg(test)]
    pub(crate) fn is_closing(&self) -> bool {
        self.flags.contains(Flags::CLOSING)
    }

    /// Whether a send is in flight, so the kernel still owns this operation.
    pub(crate) fn is_pending(&self) -> bool {
        self.flags.contains(Flags::WAITING)
    }

    pub(crate) fn pause(&mut self, overlapped: *mut OVERLAPPED) -> bool {
        if self.flags.contains(Flags::WAITING) {
            if let Err(err) = syscall!(BOOL, CancelIoEx(self.io as _, overlapped)) {
                let e = err.raw_os_error();
                if e != Some(ERROR_NOT_FOUND as _) && e != Some(ERROR_OPERATION_ABORTED as _) {
                    // The send is still live and the kernel still owns the queued
                    // pages, so fall through and wait for the completion rather
                    // than reporting the op as finished and closing under it.
                    log::error!(
                        "{}: failed to cancel send({}): {err:?}",
                        self.ctx.tag(),
                        self.io
                    );
                }
            }
            self.flags.insert(Flags::CLOSING);
            false
        } else {
            true
        }
    }

    /// Sends the unsent pages and the output in the write buffer.
    ///
    /// # Safety
    ///
    /// `this` must point to a live write op that is not borrowed for the
    /// duration of the call, and `overlapped` must be its kernel header.
    pub(crate) unsafe fn write(this: *mut Self, overlapped: *mut OVERLAPPED) {
        // SAFETY: the caller guarantees `this` is live and not borrowed, the
        // shared borrow ends before anything else runs
        let ctx = unsafe { (*this).ctx.clone() };
        // SAFETY: guaranteed by the caller
        unsafe { Self::write_with(this, &ctx, overlapped) };
    }

    /// Sends output until a send waits for the kernel or nothing is left.
    ///
    /// `IoContext` calls run filters, io callbacks and wakers, and these can
    /// write to the same stream through its `Handle`. So the op is never
    /// borrowed across them, only inside the write buffer closure, which
    /// runs after the pending output was processed.
    ///
    /// # Safety
    ///
    /// Same as for [`write`](Self::write).
    unsafe fn write_with(this: *mut Self, ctx: &IoContext, overlapped: *mut OVERLAPPED) {
        loop {
            // SAFETY: `this` is live, nothing else borrows it while the
            // temporary shared borrow exists
            if unsafe { (*this).is_pending() } {
                return;
            }
            let st = ctx.with_write_dst(|wrt| {
                // SAFETY: `this` is live, and the closure calls no code that
                // could write to the stream, so the borrow is exclusive
                let wr = unsafe { &mut *this };

                // A nested write, from a filter or callback that processed
                // pending output, may have started a send already. The op's
                // overlapped header must not be submitted twice.
                if wr.flags.contains(Flags::WAITING) {
                    return None;
                }

                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: Wrt({}) size:{:?} unsent:{:?}",
                    ctx.tag(),
                    wr.io,
                    wrt.len(),
                    wr.pages.len()
                );

                // newer output is sent after the unsent pages
                wrt.move_to(&mut wr.pages);
                Some(send(wr.io, overlapped, &mut wr.flags, &mut wr.pages))
            });
            let Some(st) = st else {
                return;
            };

            let status = ctx.update_write_status(st);
            // SAFETY: see the check at the top of the loop
            if !unsafe { (*this).resume(status) } {
                break;
            }
        }
    }

    /// Whether to send again. Unsent pages are sent even if the write buffer
    /// is empty, since they were already taken from it.
    fn resume(&self, status: IoTaskStatus) -> bool {
        match status {
            IoTaskStatus::Io => true,
            IoTaskStatus::Pause => !self.pages.is_empty(),
            IoTaskStatus::Stop => false,
        }
    }

    pub(crate) fn completed(res: io::Result<usize>, optr: *mut Overlapped) -> Option<usize> {
        // SAFETY: WR_OP identifies a live write op
        let this = unsafe { Op::<Self>::data_from_overlapped(optr) };

        let (ctx, st) = {
            // SAFETY: only the payload is borrowed, and the borrow ends before
            // the `IoContext` calls below, which can write to the stream
            let wr = unsafe { &mut *this };

            #[cfg(feature = "trace")]
            log::trace!("{}: WrtDone({}) {res:?}", wr.ctx.tag(), wr.io);

            wr.flags.remove(Flags::WAITING);
            if wr.flags.contains(Flags::CLOSING) {
                wr.pages.clear();
                return Some(wr.id);
            }
            (wr.ctx.clone(), sent(&mut wr.pages, res))
        };

        let status = ctx.update_write_status(st);
        // SAFETY: the op stays live, its item is freed only once `StreamCtl`
        // is dropped, and the io task that owns it does not run from here.
        // No borrow of the op is held.
        if unsafe { (*this).resume(status) } {
            // SAFETY: as above, `optr` is the op's kernel header
            unsafe { Self::write_with(this, &ctx, optr.cast()) };
        }
        None
    }
}

/// Sends up to `MAX_WRITE_BUFS` pages and `MAX_WRITE_SIZE` bytes.
fn send(
    io: RawSocket,
    overlapped: *mut OVERLAPPED,
    flags: &mut Flags,
    pages: &mut BytePages,
) -> io::Result<usize> {
    let mut bufs = [mem::MaybeUninit::<WSABUF>::uninit(); MAX_WRITE_BUFS];
    let mut num = 0;
    let mut size = 0;
    for chunk in pages.iter() {
        let len = cmp::min(chunk.len(), MAX_WRITE_SIZE - size);
        bufs[num].write(WSABUF {
            len: len as u32,
            buf: chunk.as_ptr().cast_mut(),
        });
        num += 1;
        size += len;
        if num == MAX_WRITE_BUFS || size == MAX_WRITE_SIZE {
            break;
        }
    }
    if num == 0 {
        return Ok(0);
    }

    let mut written = 0;
    // SAFETY: page data is stored on the heap, and the pages are not
    // modified until the send completes. WSASend copies the descriptors.
    let result = unsafe {
        WSASend(
            io as _,
            bufs.as_ptr().cast(),
            num as u32,
            &raw mut written,
            0,
            overlapped,
            None,
        )
    };

    match winsock_result(result) {
        Poll::Ready(Ok(())) => sent(pages, Ok(written as usize)),
        Poll::Ready(Err(err)) => sent(pages, Err(err)),
        Poll::Pending => {
            flags.insert(Flags::WAITING);
            Ok(0)
        }
    }
}

/// Removes sent bytes from the unsent pages. After an error nothing is
/// sent anymore, so the pages are dropped.
fn sent(pages: &mut BytePages, res: io::Result<usize>) -> io::Result<usize> {
    match res {
        Ok(0) => {
            pages.clear();
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write frame to transport",
            ))
        }
        Ok(written) => {
            pages.advance_to(cmp::min(written, pages.len()));
            Ok(written)
        }
        Err(err) => {
            pages.clear();
            Err(err)
        }
    }
}

pub(crate) fn winapi_result() -> Poll<io::Result<()>> {
    // SAFETY: `GetLastError` reads only the calling thread's error state.
    let error = unsafe { GetLastError() };
    match error {
        ERROR_IO_PENDING => Poll::Pending,
        ERROR_IO_INCOMPLETE
        | ERROR_HANDLE_EOF
        | ERROR_BROKEN_PIPE
        | ERROR_PIPE_CONNECTED
        | ERROR_PIPE_NOT_CONNECTED
        | ERROR_NO_DATA
        | ERROR_MORE_DATA => Poll::Ready(Ok(())),
        _ => Poll::Ready(Err(io::Error::from_raw_os_error(error.cast_signed()))),
    }
}

pub(crate) fn win32_result(res: i32) -> Poll<io::Result<()>> {
    if res == 0 {
        winapi_result()
    } else {
        Poll::Ready(Ok(()))
    }
}

fn winsock_result(res: i32) -> Poll<io::Result<()>> {
    if res != 0 {
        winapi_result()
    } else {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicPtr, atomic::Ordering};
    use std::task::{Context, Wake, Waker};
    use std::{cell::RefCell, rc::Rc};

    use ntex_io::{Handle, Io, IoCallbacks, IoContext, IoRef, IoStream};
    use ntex_service::cfg::SharedCfg;

    use super::*;
    use crate::iocp::OpBox;

    struct TestStream(Rc<RefCell<Option<IoContext>>>);

    struct TestHandle;

    impl Handle for TestHandle {}

    impl IoStream for TestStream {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            *self.0.borrow_mut() = Some(ctx);
            Box::new(TestHandle)
        }
    }

    /// Returns a connection, its transport context, and a write op for it.
    fn write_op() -> (Io, IoContext, OpBox<WriteOperation>) {
        let slot = Rc::new(RefCell::new(None));
        let io = Io::new(TestStream(Rc::clone(&slot)), SharedCfg::default());
        let ctx = slot.borrow_mut().take().unwrap();
        let op = OpBox::new(
            Overlapped::new(0, WR_OP),
            WriteOperation::new(0, 0, ctx.clone()),
        );
        (io, ctx, op)
    }

    /// Writes to the op once, as a nested write that submitted a send does:
    /// it takes pages and waits for the kernel.
    struct NestedSend(AtomicPtr<WriteOperation>);

    impl NestedSend {
        fn new(op: &OpBox<WriteOperation>) -> Self {
            Self(AtomicPtr::new(op.as_ptr()))
        }

        fn send(&self) {
            let this = self.0.swap(ptr::null_mut(), Ordering::Relaxed);
            if !this.is_null() {
                // SAFETY: the test keeps the op alive, and the code under test
                // holds no borrow of it while it calls into the io layer
                let wr = unsafe { &mut *this };
                wr.pages.extend_from_slice(b"nested");
                wr.flags.insert(Flags::WAITING);
            }
        }
    }

    impl IoCallbacks for NestedSend {
        fn before_processing(&self, _: &IoRef) {
            self.send();
        }

        fn after_processing(&self, _: &IoRef) {}
    }

    impl Wake for NestedSend {
        fn wake(self: Arc<Self>) {
            self.send();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.send();
        }
    }

    /// STATEMENT: A send that a nested write starts while pending output is
    /// processed is not followed by a second send with the same op.
    ///
    /// HOW:
    /// - Queue output, and register io callbacks that simulate a nested write
    ///   submitting a send once the output is processed.
    /// - Call `WriteOperation::write` and check that the op still waits with
    ///   only the nested send's pages, and the output stays in the buffer.
    /// - Under Miri this also checks that the nested write does not alias a
    ///   borrow of the op.
    #[test]
    fn miri_nested_send_during_processing_is_not_resubmitted() {
        let (io, ctx, op) = write_op();
        ctx.with_write_dst(|buf| buf.extend_from_slice(b"first"));
        io.register_filter_callbacks(NestedSend::new(&op));

        // SAFETY: the op is live and not borrowed
        unsafe { WriteOperation::write(op.as_ptr(), op.as_overlapped()) };

        assert!(op.is_pending());
        assert_eq!(op.unsent(), b"nested".len());
        assert_eq!(ctx.with_write_dst(|buf| buf.len()), b"first".len());
    }

    /// STATEMENT: A send completion holds no borrow of the op while waking
    /// tasks, which may write to the stream, and the op does not resubmit
    /// over the send such a write started.
    ///
    /// HOW:
    /// - Mark a send of the queued output in flight, and wait for a full
    ///   flush with a waker that simulates a nested write submitting a send.
    /// - Complete the first send, which drains the output and wakes the
    ///   flush waiter.
    /// - Check that the op waits with the nested send's pages. Under Miri
    ///   this checks that the nested write does not alias a borrow of the op.
    #[test]
    fn miri_nested_send_on_completion_does_not_alias() {
        let (io, ctx, mut op) = write_op();
        ctx.with_write_dst(|buf| buf.extend_from_slice(b"first"));
        ctx.with_write_dst(|buf| buf.move_to(&mut op.pages));
        op.fake_pending();

        let waker = Waker::from(Arc::new(NestedSend::new(&op)));
        let mut cx = Context::from_waker(&waker);
        assert!(io.poll_flush(&mut cx, true).is_pending());

        let optr = op.as_overlapped().cast();
        assert_eq!(WriteOperation::completed(Ok(b"first".len()), optr), None);

        assert!(op.is_pending());
        assert_eq!(op.unsent(), b"nested".len());
    }
}
