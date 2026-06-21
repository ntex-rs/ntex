use std::{cell::UnsafeCell, collections::VecDeque, io, marker::PhantomData, net, rc::Rc};

use socket2::Socket;

pub(crate) fn prep_socket(sock: Socket) -> io::Result<Socket> {
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
        let _ = sock.shutdown(net::Shutdown::Both);
        drop(sock);
    });
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
