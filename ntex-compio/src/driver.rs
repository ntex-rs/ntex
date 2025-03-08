use std::collections::VecDeque;
use std::task::{ready, Poll, Waker};
use std::{cell::Cell, cell::RefCell, fmt, io, ptr, rc::Rc};

use compio_driver::op::{Handler, Interest};
use compio_driver::{syscall, AsRawFd, DriverApi, RawFd};
use compio_net::TcpStream;
use compio_runtime::Runtime;
use slab::Slab;

use ntex_bytes::BufMut;
use ntex_io::{ReadContext, WriteContext};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const ERROR = 0b0000_0001;
    }
}

pub(crate) struct StreamCtl {
    id: usize,
    inner: Rc<CompioOpsInner>,
}

struct TcpStreamItem {
    io: Option<TcpStream>,
    fd: RawFd,
    read: ReadContext,
    write: WriteContext,
    flags: Cell<Flags>,
    waker: Cell<Option<Waker>>,
    ref_count: usize,
}

#[derive(Clone)]
pub(crate) struct CompioOps(Rc<CompioOpsInner>);

enum Change {
    Readable,
    Writable,
    Error(io::Error),
}

struct CompioOpsBatcher {
    feed: VecDeque<(usize, Change)>,
    inner: Rc<CompioOpsInner>,
}

struct CompioOpsInner {
    api: DriverApi,
    streams: RefCell<Slab<TcpStreamItem>>,
}

impl CompioOps {
    pub(crate) fn current() -> Self {
        Runtime::with_current(|rt| {
            if let Some(s) = rt.get::<Self>() {
                s
            } else {
                let mut inner = None;
                rt.driver().register_handler(|api| {
                    let ops = Rc::new(CompioOpsInner {
                        api,
                        streams: RefCell::new(Slab::new()),
                    });
                    inner = Some(ops.clone());
                    Box::new(CompioOpsBatcher {
                        inner: ops,
                        feed: VecDeque::new(),
                    })
                });

                let s = CompioOps(inner.unwrap());
                rt.insert(s.clone());
                s
            }
        })
    }

    pub(crate) fn register(
        &self,
        io: TcpStream,
        read: ReadContext,
        write: WriteContext,
    ) -> StreamCtl {
        let item = TcpStreamItem {
            read,
            write,
            fd: io.as_raw_fd(),
            io: Some(io),
            flags: Cell::new(Flags::empty()),
            waker: Cell::new(None),
            ref_count: 1,
        };
        let id = self.0.streams.borrow_mut().insert(item);
        StreamCtl {
            id,
            inner: self.0.clone(),
        }
    }
}

impl Handler for CompioOpsBatcher {
    fn readable(&mut self, id: usize) {
        log::debug!("FD is readable {:?}", id);
        self.feed.push_back((id, Change::Readable));
    }

    fn writable(&mut self, id: usize) {
        log::debug!("FD is writable {:?}", id);
        self.feed.push_back((id, Change::Writable));
    }

    fn error(&mut self, id: usize, err: io::Error) {
        log::debug!("FD is failed {:?}, err: {:?}", id, err);
        self.feed.push_back((id, Change::Error(err)));
    }

    fn commit(&mut self) {
        if self.feed.is_empty() {
            return;
        }
        log::debug!("Commit changes, num: {:?}", self.feed.len());

        let streams = &self.inner.streams;

        for (id, change) in self.feed.drain(..) {
            match change {
                Change::Readable => {
                    let item = streams
                        .borrow()
                        .get(id)
                        .map(|item| (item.fd, item.read.clone()));
                    if let Some((fd, read)) = item {
                        let result = read.with_buf(|buf| {
                            let chunk = buf.chunk_mut();
                            let b = chunk.as_mut_ptr();
                            Poll::Ready(
                                ready!(syscall!(break libc::read(fd, b as _, chunk.len())))
                                    .inspect(|size| {
                                        unsafe { buf.advance_mut(*size) };
                                        log::debug!("FD: {:?}, BUF: {:?}", fd, buf);
                                    }),
                            )
                        });

                        if result.is_pending() {
                            self.inner.api.register(fd, id, Interest::Readable);
                        }
                    }
                }
                Change::Writable => {
                    let item = streams
                        .borrow()
                        .get(id)
                        .map(|item| (item.fd, item.write.clone()));
                    if let Some((fd, write)) = item {
                        let result = write.with_buf(|buf| {
                            let slice = &buf[..];
                            syscall!(
                                break libc::write(fd, slice.as_ptr() as _, slice.len())
                            )
                        });

                        if result.is_ready() {
                            self.inner.api.unregister(fd, Interest::Writable);
                        }
                    }
                }
                Change::Error(err) => {
                    if let Some(item) = streams.borrow().get(id) {
                        item.read.set_stopped(Some(err));
                        let mut flags = item.flags.get();
                        if !flags.contains(Flags::ERROR) {
                            flags.insert(Flags::ERROR);
                            item.flags.set(flags);
                            self.inner.api.unregister_all(item.fd);
                        }
                    }
                }
            }
        }
    }
}

impl StreamCtl {
    pub(crate) fn take_io(&self) -> Option<TcpStream> {
        self.inner.streams.borrow_mut()[self.id].io.take()
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&TcpStream>) -> R,
    {
        f(self.inner.streams.borrow()[self.id].io.as_ref())
    }

    pub(crate) fn register(&self, waker: &Waker) {
        self.inner.streams.borrow()[self.id]
            .waker
            .set(Some(waker.clone()));
    }

    pub(crate) fn pause_all(&self) {
        log::debug!(
            "Pause all io ({}), {:?}",
            self.id,
            self.inner.streams.borrow()[self.id].fd
        );
        self.inner
            .api
            .unregister_all(self.inner.streams.borrow()[self.id].fd);
    }

    pub(crate) fn pause_read(&self) {
        log::debug!(
            "Pause io read ({}), {:?}",
            self.id,
            self.inner.streams.borrow()[self.id].fd
        );
        self.inner
            .api
            .unregister(self.inner.streams.borrow()[self.id].fd, Interest::Readable);
    }

    pub(crate) fn resume_read(&self) {
        log::debug!(
            "Resume io read ({}), {:?}",
            self.id,
            self.inner.streams.borrow()[self.id].fd
        );
        self.inner.api.register(
            self.inner.streams.borrow()[self.id].fd,
            self.id,
            Interest::Readable,
        );
    }

    pub(crate) fn resume_write(&self) {
        log::debug!(
            "Resume io write ({}), {:?}",
            self.id,
            self.inner.streams.borrow()[self.id].fd
        );
        let item = &self.inner.streams.borrow()[self.id];
        let result = item.write.with_buf(|buf| {
            log::debug!("Writing io ({}), buf: {:?}", self.id, buf.len());

            let slice = &buf[..];
            syscall!(break libc::write(item.fd, slice.as_ptr() as _, slice.len()))
        });

        if result.is_pending() {
            log::debug!(
                "Write is pending ({}), {:?}",
                self.id,
                item.read.io().flags()
            );

            self.inner
                .api
                .register(item.fd, self.id, Interest::Writable);
        }
    }
}

impl Clone for StreamCtl {
    fn clone(&self) -> StreamCtl {
        let mut streams = self.inner.streams.borrow_mut();
        streams[self.id].ref_count += 1;
        Self {
            id: self.id,
            inner: self.inner.clone(),
        }
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        log::debug!(
            "Drop io ({}), {:?}",
            self.id,
            self.inner.streams.borrow()[self.id].fd
        );

        let mut streams = self.inner.streams.borrow_mut();
        streams[self.id].ref_count -= 1;

        if streams[self.id].ref_count == 0 {
            let item = streams.remove(self.id);
            if item.io.is_some() {
                self.inner.api.unregister_all(item.fd);
            }
        }
    }
}

impl PartialEq for StreamCtl {
    #[inline]
    fn eq(&self, other: &StreamCtl) -> bool {
        self.id == other.id && ptr::eq(&self.inner, &other.inner)
    }
}

impl fmt::Debug for StreamCtl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCtl")
            .field("id", &self.id)
            .field("io", &self.inner.streams.borrow()[self.id].io)
            .finish()
    }
}
