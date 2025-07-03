use std::{cell::RefCell, io, os::fd::RawFd, rc::Rc, task::Poll};

use ntex_neon::driver::{DriverApi, Event, Handler};
use ntex_neon::{syscall, Runtime};
use ntex_util::channel::oneshot::Sender;
use slab::Slab;
use socket2::SockAddr;

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

#[derive(Debug)]
enum Change {
    Event(Event),
    Error(io::Error),
}

struct ConnectOpsBatcher {
    inner: Rc<ConnectOpsInner>,
}

struct Item {
    fd: RawFd,
    sender: Sender<io::Result<()>>,
}

struct ConnectOpsInner {
    api: DriverApi,
    connects: RefCell<Slab<Item>>,
}

impl ConnectOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.register_handler(|api| {
                let ops = Rc::new(ConnectOpsInner {
                    api,
                    connects: RefCell::new(Slab::new()),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsBatcher { inner: ops })
            });

            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(
        &self,
        fd: RawFd,
        addr: SockAddr,
        sender: Sender<io::Result<()>>,
    ) -> io::Result<usize> {
        let result = syscall!(break libc::connect(fd, addr.as_ptr(), addr.len()));

        if let Poll::Ready(res) = result {
            res?;
        }

        let item = Item { fd, sender };
        let id = self.0.connects.borrow_mut().insert(item);

        self.0.api.attach(fd, id as u32, Event::writable(0));
        Ok(id)
    }
}

impl Handler for ConnectOpsBatcher {
    fn event(&mut self, id: usize, event: Event) {
        log::trace!("connect-fd is readable {id:?}");

        let mut connects = self.inner.connects.borrow_mut();
        if let Some(item) = connects.get(id) {
            if event.writable {
                let item = connects.remove(id);
                let mut err: libc::c_int = 0;
                let mut err_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;

                let res = syscall!(libc::getsockopt(
                    item.fd,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    &mut err as *mut _ as *mut _,
                    &mut err_len
                ));

                let res = if err == 0 {
                    res.map(|_| ())
                } else {
                    Err(io::Error::from_raw_os_error(err))
                };

                self.inner.api.detach(item.fd, id as u32);
                let _ = item.sender.send(res);
            } else if !item.sender.is_canceled() {
                self.inner
                    .api
                    .attach(item.fd, id as u32, Event::writable(0));
            }
        }
    }

    fn error(&mut self, id: usize, err: io::Error) {
        let mut connects = self.inner.connects.borrow_mut();

        if connects.contains(id) {
            let item = connects.remove(id);
            let _ = item.sender.send(Err(err));
            self.inner.api.detach(item.fd, id as u32);
        }
    }
}
