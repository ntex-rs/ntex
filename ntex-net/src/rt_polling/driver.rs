use std::os::fd::RawFd;
use std::{cell::Cell, cell::RefCell, future::Future, io, rc::Rc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::IoContext;
use ntex_neon::driver::{DriverApi, Event, Handler};
use ntex_neon::{syscall, Runtime};
use slab::Slab;

pub(crate) struct StreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const RD     = 0b0000_0001;
        const WR     = 0b0000_0010;
        const CLOSED = 0b0001_0000;
    }
}

struct StreamItem {
    fd: RawFd,
    flags: Cell<Flags>,
    ref_count: u16,
    context: IoContext,
}

pub(crate) struct StreamOps(Rc<StreamOpsInner>);

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

struct StreamOpsInner {
    api: DriverApi,
    delayd_drop: Cell<bool>,
    feed: RefCell<Vec<u32>>,
    streams: Cell<Option<Box<Slab<StreamItem>>>>,
}

impl StreamOps {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.register_handler(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    feed: RefCell::new(Vec::new()),
                    delayd_drop: Cell::new(false),
                    streams: Cell::new(Some(Box::new(Slab::new()))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    pub(crate) fn active_ops() -> usize {
        Self::current().0.with(|streams| streams.len())
    }

    pub(crate) fn register(&self, fd: RawFd, context: IoContext) -> StreamCtl {
        let stream = self.0.with(move |streams| {
            let item = StreamItem {
                fd,
                context,
                ref_count: 1,
                flags: Cell::new(Flags::empty()),
            };
            StreamCtl {
                id: streams.insert(item) as u32,
                inner: self.0.clone(),
            }
        });

        self.0
            .api
            .attach(fd, stream.id, Event::new(0, false, false));
        stream
    }
}

impl Clone for StreamOps {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl Handler for StreamOpsHandler {
    fn event(&mut self, id: usize, ev: Event) {
        self.inner.with(|streams| {
            if !streams.contains(id) {
                return;
            }
            let io = &mut streams[id];
            log::trace!(
                "{}: Event ({:?},open:{}): {:?}",
                io.tag(),
                io.fd,
                io.is_open(),
                ev
            );
            if !io.is_open() {
                return;
            }
            let mut renew = Event::new(0, false, false);

            if ev.readable {
                if io.read(id as u32, &self.inner.api).is_pending() && io.can_read() {
                    renew.readable = true;
                    io.insert_flag(Flags::RD);
                } else {
                    io.remove_flag(Flags::RD);
                }
            } else if io.contains_flag(Flags::RD) {
                renew.readable = true;
            }

            if ev.writable && io.is_open() {
                if io.write(id as u32, &self.inner.api).is_pending() {
                    renew.writable = true;
                    io.insert_flag(Flags::WR);
                } else {
                    io.remove_flag(Flags::WR);
                }
            } else if io.contains_flag(Flags::WR) {
                renew.writable = true;
            }

            // handle HUP
            if ev.is_interrupt() {
                io.context.stopped(None);
                io.close(id as u32, &self.inner.api, false);
                return;
            }

            // register Event in driver
            if io.is_open() {
                self.inner.api.modify(io.fd, id as u32, renew);
            }

            // delayed drops
            if self.inner.delayd_drop.get() {
                self.inner.delayd_drop.set(false);
                for id in self.inner.feed.borrow_mut().drain(..) {
                    let io = &mut streams[id as usize];
                    io.ref_count -= 1;
                    if io.ref_count == 0 {
                        let mut io = streams.remove(id as usize);
                        io.close(id, &self.inner.api, true);
                        log::trace!("{}: Drop ({:?})", io.tag(), io.fd);
                    }
                }
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(io) = streams.get_mut(id) {
                log::trace!("{}: Failed ({:?}) err: {:?}", io.tag(), io.fd, err);
                if !io.context.is_stopped() {
                    io.context.stopped(Some(err));
                }
                io.close(id as u32, &self.inner.api, false);
            }
        })
    }
}

impl StreamOpsInner {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<StreamItem>) -> R,
    {
        let mut streams = self.streams.take().unwrap();
        let result = f(&mut streams);
        self.streams.set(Some(streams));
        result
    }
}

impl StreamCtl {
    pub(crate) fn close(self) -> impl Future<Output = io::Result<()>> {
        let id = self.id as usize;
        let fut = self
            .inner
            .with(|streams| streams[id].close(self.id, &self.inner.api, true));
        async move {
            if let Some(fut) = fut {
                fut.await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                    .and_then(crate::helpers::pool_io_err)?;
            }
            Ok(())
        }
    }

    pub(crate) fn modify(&self, rd: bool, wr: bool) -> bool {
        self.inner.with(|streams| {
            let io = &mut streams[self.id as usize];
            log::trace!(
                "{}: Modify ({:?}) rd: {:?}, wr: {:?}, flags: {:?}",
                io.tag(),
                io.fd,
                rd,
                wr,
                io.flags.get()
            );

            if !io.is_open() {
                return false;
            }
            let mut event = Event::new(0, false, false);

            if rd {
                if io.contains_flag(Flags::RD) {
                    event.readable = true;
                } else if io.read(self.id, &self.inner.api).is_pending() {
                    event.readable = true;
                    io.insert_flag(Flags::RD);
                }
            } else {
                io.remove_flag(Flags::RD);
            }

            if wr && io.is_open() {
                if io.contains_flag(Flags::WR) {
                    event.writable = true;
                } else if io.write(self.id, &self.inner.api).is_pending() {
                    event.writable = true;
                    io.insert_flag(Flags::WR);
                }
            } else {
                io.remove_flag(Flags::WR);
            }

            if io.is_open() {
                self.inner.api.modify(io.fd, self.id, event);
                true
            } else {
                false
            }
        })
    }
}

impl Clone for StreamCtl {
    fn clone(&self) -> Self {
        self.inner.with(|streams| {
            streams[self.id as usize].ref_count += 1;
            Self {
                id: self.id,
                inner: self.inner.clone(),
            }
        })
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        if let Some(mut streams) = self.inner.streams.take() {
            let id = self.id as usize;
            streams[id].ref_count -= 1;
            if streams[id].ref_count == 0 {
                let mut io = streams.remove(id);
                log::trace!(
                    "{}: Drop io ({:?}), flags: {:?}",
                    io.tag(),
                    io.fd,
                    io.flags.get()
                );
                io.close(self.id, &self.inner.api, true);
            }
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayd_drop.set(true);
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}

impl StreamItem {
    fn tag(&self) -> &'static str {
        self.context.tag()
    }

    fn is_open(&self) -> bool {
        !self.flags.get().contains(Flags::CLOSED)
    }

    fn can_read(&self) -> bool {
        self.context.is_read_ready()
    }

    fn contains_flag(&self, f: Flags) -> bool {
        self.flags.get().contains(f)
    }

    fn insert_flag(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flag(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
    }

    fn write(&mut self, id: u32, api: &DriverApi) -> Poll<()> {
        let mut close = false;
        let result = self.context.with_write_buf(|buf| {
            log::trace!(
                "{}: Writing ({}), buf: {:?}",
                self.tag(),
                self.fd,
                buf.len()
            );
            let res =
                syscall!(break libc::write(self.fd, buf[..].as_ptr() as _, buf.len()));
            close = matches!(res, Poll::Ready(Err(_)));
            res
        });

        if close {
            self.close(id, api, false);
        }
        result
    }

    fn read(&mut self, id: u32, api: &DriverApi) -> Poll<()> {
        let mut close = false;

        let result = self.context.with_read_buf(|buf, hw, lw| {
            let mut total = 0;
            loop {
                // make sure we've got room
                if buf.remaining_mut() < lw {
                    buf.reserve(hw);
                }

                let chunk = buf.chunk_mut();
                let chunk_len = chunk.len();
                let chunk_ptr = chunk.as_mut_ptr();

                let res = syscall!(break libc::read(self.fd, chunk_ptr as _, chunk_len));
                if let Poll::Ready(Ok(size)) = res {
                    unsafe { buf.advance_mut(size) };
                    total += size;
                    if size == chunk_len {
                        continue;
                    }
                }

                log::trace!(
                    "{}: Read fd ({:?}), size: {:?}, cap: {:?}, result: {:?}",
                    self.tag(),
                    self.fd,
                    total,
                    buf.remaining_mut(),
                    res
                );

                return match res {
                    Poll::Ready(Err(err)) => {
                        close = true;
                        (total, Poll::Ready(Err(err)))
                    }
                    Poll::Ready(Ok(size)) => {
                        if size == 0 {
                            close = true;
                        }
                        (total, Poll::Ready(Ok(())))
                    }
                    Poll::Pending => (total, Poll::Pending),
                };
            }
        });

        if close {
            self.close(id, api, false);
        }
        result
    }

    fn close(
        &mut self,
        id: u32,
        api: &DriverApi,
        shutdown: bool,
    ) -> Option<ntex_rt::JoinHandle<io::Result<i32>>> {
        if self.is_open() {
            log::trace!(
                "{}: Closing ({}) sh: {:?}, ctx: {:?}",
                self.tag(),
                self.fd,
                shutdown,
                self.context.flags()
            );
            self.insert_flag(Flags::CLOSED);

            let fd = self.fd;
            api.detach(fd, id);
            Some(ntex_rt::spawn_blocking(move || {
                if shutdown {
                    let _ = syscall!(libc::shutdown(fd, libc::SHUT_RDWR));
                }
                syscall!(libc::close(fd))
            }))
        } else {
            None
        }
    }
}
