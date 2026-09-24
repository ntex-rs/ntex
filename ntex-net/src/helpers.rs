use std::{cell::UnsafeCell, collections::VecDeque, marker::PhantomData, rc::Rc};

use socket2::Socket;

#[cfg(unix)]
pub(crate) fn prep_socket(sock: Socket) -> std::io::Result<Socket> {
    #[cfg(not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "fuchsia",
        target_os = "hurd",
        target_os = "illumos",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "espidf",
        target_os = "vita",
    )))]
    sock.set_cloexec(true)?;
    #[cfg(any(
        target_os = "ios",
        target_os = "macos",
        target_os = "tvos",
        target_os = "watchos",
    ))]
    sock.set_nosigpipe(true)?;
    sock.set_nonblocking(true)?;

    Ok(sock)
}

pub(crate) fn close_socket(sock: Socket) {
    ntex_rt::spawn_blocking(move || {
        let _ = sock.shutdown(std::net::Shutdown::Both);
        drop(sock);
    })
    .detach();
}

/// Arranges for a socket to be aborted rather than closed gracefully.
///
/// Sets `SO_LINGER` to zero, which makes the following `close()` discard
/// whatever has not reached the peer yet and send an RST instead of a FIN.
/// Without this a force-closed connection still ends with a normal FIN, so a
/// truncated response is indistinguishable from a complete one.
///
/// A failure is ignored: the connection is being torn down either way, and the
/// only consequence is that the peer sees a graceful close.
pub(crate) fn abort_socket(sock: &socket2::SockRef<'_>) {
    let _ = sock.set_linger(Some(std::time::Duration::ZERO));
}

/// Arranges for a socket referenced by its raw handle to be aborted.
///
/// See [`abort_socket()`].
#[cfg(unix)]
pub(crate) fn abort_raw_socket(fd: std::os::fd::RawFd) {
    // SAFETY: the fd is owned by the caller and outlives the borrow
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    abort_socket(&socket2::SockRef::from(&fd));
}

/// Arranges for a socket referenced by its raw handle to be aborted.
///
/// See [`abort_socket()`].
#[cfg(windows)]
pub(crate) fn abort_raw_socket(socket: std::os::windows::io::RawSocket) {
    // SAFETY: the socket is owned by the caller and outlives the borrow
    let socket = unsafe { std::os::windows::io::BorrowedSocket::borrow_raw(socket) };
    abort_socket(&socket2::SockRef::from(&socket));
}

/// Maximum amount of input discarded by [`drain_socket()`], in 4kB chunks.
const MAX_DRAIN_CHUNKS: usize = 16;

/// Discards whatever is left in the socket receive queue.
///
/// Closing a socket whose receive queue is not empty makes the kernel abort the
/// connection with an RST instead of finishing the normal FIN handshake, and an
/// RST discards output that has not reached the peer yet. The connection is
/// shut down only once its buffered output has been handed to the transport, so
/// aborting at that point would lose exactly the output the graceful shutdown
/// just drained.
///
/// This is a best effort: the socket is switched to non-blocking mode and read
/// until it is empty or [`MAX_DRAIN_CHUNKS`] have been discarded, so a peer that
/// keeps sending cannot hold the shutdown up. Input that arrives after the last
/// read can still abort the connection, that race cannot be closed.
///
/// On a completion based backend a read operation can still be in flight when
/// this runs, so the two may end up sharing the queue between them. That is
/// harmless while both run on the thread that drives the connection, input is
/// discarded in this phase either way, and a backend that can cheaply tell an
/// operation is in flight is free to leave the draining to it instead. What
/// must not happen is draining from another thread while the reactor can still
/// read the socket: either run this on the thread that drives the connection,
/// or only once no read operation can be in flight or be started anymore.
pub(crate) fn drain_socket(sock: &socket2::SockRef<'_>) {
    if sock.set_nonblocking(true).is_err() {
        return;
    }

    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
    for _ in 0..MAX_DRAIN_CHUNKS {
        // a zero-length read is a clean eof, an error is either `WouldBlock`
        // on an empty queue or a peer that is gone; nothing is left either way
        match sock.recv(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => (),
        }
    }
}

/// Drains the receive queue of a socket referenced by its raw handle.
#[cfg(unix)]
pub(crate) fn drain_raw_socket(fd: std::os::fd::RawFd) {
    // SAFETY: the fd is owned by the caller and outlives the borrow
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    drain_socket(&socket2::SockRef::from(&fd));
}

/// Drains the receive queue of a socket referenced by its raw handle.
#[cfg(windows)]
pub(crate) fn drain_raw_socket(socket: std::os::windows::io::RawSocket) {
    // SAFETY: the socket is owned by the caller and outlives the borrow
    let socket = unsafe { std::os::windows::io::BorrowedSocket::borrow_raw(socket) };
    drain_socket(&socket2::SockRef::from(&socket));
}

#[derive(Default)]
pub(crate) struct Queue<T> {
    inner: UnsafeCell<VecDeque<T>>,
    _t: PhantomData<Rc<()>>,
}

impl<T> Queue<T> {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(VecDeque::default()),
            _t: PhantomData,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        // SAFETY: Queue is !Sync and it does not allow to hold refs into inner
        unsafe { &*self.inner.get() }.is_empty()
    }

    pub(crate) fn clear(&self) {
        // SAFETY: Queue is !Sync and it does not allow to hold refs into inner
        unsafe { &mut *self.inner.get() }.clear();
    }

    pub(crate) fn pop(&self) -> Option<T> {
        // SAFETY: Queue is !Sync and it does not allow to hold refs into inner
        unsafe { &mut *self.inner.get() }.pop_front()
    }

    pub(crate) fn push(&self, item: T) {
        // SAFETY: Queue is !Sync and it does not allow to hold refs into inner
        unsafe { &mut *self.inner.get() }.push_back(item);
    }
}
