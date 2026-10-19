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

    pub(crate) fn write(&mut self, overlapped: *mut OVERLAPPED) {
        loop {
            if self.flags.contains(Flags::WAITING) {
                return;
            }
            let Self {
                io,
                ctx,
                flags,
                pages,
                ..
            } = self;
            let st = ctx.with_write_dst(|wrt| {
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: Wrt({io}) size:{:?} unsent:{:?}",
                    ctx.tag(),
                    wrt.len(),
                    pages.len()
                );

                // newer output is sent after the unsent pages
                wrt.move_to(pages);
                send(*io, overlapped, flags, pages)
            });

            let status = self.ctx.update_write_status(st);
            if !self.resume(status) {
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
        // SAFETY: WR_OP identifies a live write op; only its payload is borrowed
        let wr = unsafe { &mut *Op::<Self>::data_from_overlapped(optr) };

        #[cfg(feature = "trace")]
        log::trace!("{}: WrtDone({}) {res:?}", wr.ctx.tag(), wr.io);

        wr.flags.remove(Flags::WAITING);
        if wr.flags.contains(Flags::CLOSING) {
            wr.pages.clear();
            return Some(wr.id);
        }

        let st = sent(&mut wr.pages, res);
        let status = wr.ctx.update_write_status(st);
        if wr.resume(status) {
            wr.write(optr.cast());
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
