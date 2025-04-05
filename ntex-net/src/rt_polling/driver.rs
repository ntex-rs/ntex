use std::os::fd::RawFd;
use std::{cell::Cell, cell::RefCell, future::Future, io, rc::Rc, task::Poll};

use ntex_neon::driver::{DriverApi, Event, Handler};
use ntex_neon::{syscall, Runtime};
use slab::Slab;

use ntex_bytes::BufMut;
use ntex_io::IoContext;

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
    flags: Flags,
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
            rt.driver().register(|api| {
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
                flags: Flags::empty(),
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
            let item = &mut streams[id];
            if item.flags.contains(Flags::CLOSED) {
                return;
            }

            log::trace!("{}: Event ({:?}): {:?}", item.tag(), item.fd, ev);

            let mut flags = item.flags;
            let mut renew = Event::new(0, false, false);
            if ev.readable {
                if item
                    .read(&mut flags, id as u32, &self.inner.api)
                    .is_pending()
                    && item.can_read()
                {
                    renew.readable = true;
                    flags.insert(Flags::RD);
                } else {
                    flags.remove(Flags::RD);
                }
            } else if flags.contains(Flags::RD) {
                renew.readable = true;
            }

            if ev.writable {
                let result = item.context.with_write_buf(|buf| {
                    log::trace!("{}: write {:?} s: {:?}", item.tag(), item.fd, buf.len());
                    syscall!(break libc::write(item.fd, buf[..].as_ptr() as _, buf.len()))
                });
                if result.is_pending() {
                    renew.writable = true;
                    flags.insert(Flags::WR);
                } else {
                    flags.remove(Flags::WR);
                }
            } else if flags.contains(Flags::WR) {
                renew.writable = true;
            }
            item.flags = flags;

            // handle HUP
            if ev.is_interrupt() {
                item.close(id as u32, &self.inner.api, None, false);
                return;
            }

            // register Event in driver
            if !item.flags.intersects(Flags::CLOSED) {
                self.inner.api.modify(item.fd, id as u32, renew);
            }

            // delayed drops
            if self.inner.delayd_drop.get() {
                for id in self.inner.feed.borrow_mut().drain(..) {
                    let item = &mut streams[id as usize];
                    item.ref_count -= 1;
                    if item.ref_count == 0 {
                        let mut item = streams.remove(id as usize);
                        log::trace!(
                            "{}: Drop ({:?}), flags: {:?}",
                            item.tag(),
                            item.fd,
                            item.flags
                        );
                        item.close(id, &self.inner.api, None, true);
                    }
                }
                self.inner.delayd_drop.set(false);
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(item) = streams.get_mut(id) {
                log::trace!(
                    "{}: FD is failed ({:?}) err: {:?}",
                    item.tag(),
                    item.fd,
                    err
                );
                item.close(id as u32, &self.inner.api, Some(err), false);
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

impl StreamItem {
    fn tag(&self) -> &'static str {
        self.context.tag()
    }

    fn can_read(&self) -> bool {
        self.context.is_read_ready()
    }

    fn read(&mut self, flags: &mut Flags, id: u32, api: &DriverApi) -> Poll<()> {
        let mut close = false;

        let result = self.context.with_read_buf(|buf, hw, lw| {
            let mut total = 0;
            loop {
                // make sure we've got room
                let remaining = buf.remaining_mut();
                if remaining < lw {
                    buf.reserve(hw - remaining);
                }

                let chunk = buf.chunk_mut();
                let chunk_len = chunk.len();
                let chunk_ptr = chunk.as_mut_ptr();

                let result =
                    syscall!(break libc::read(self.fd, chunk_ptr as _, chunk.len()));
                if let Poll::Ready(Ok(size)) = result {
                    unsafe { buf.advance_mut(size) };
                    total += size;
                    //if size == chunk_len {
                    //    continue;
                    //}
                }

                log::trace!(
                    "{}: Read fd ({:?}), s: {:?}, cap: {:?}, result: {:?}",
                    self.tag(),
                    self.fd,
                    total,
                    buf.remaining_mut(),
                    result
                );

                return match result {
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
            self.close(id, api, None, false);
        }
        result
    }

    fn close(
        &mut self,
        id: u32,
        api: &DriverApi,
        error: Option<io::Error>,
        shutdown: bool,
    ) -> Option<ntex_rt::JoinHandle<io::Result<i32>>> {
        if !self.flags.contains(Flags::CLOSED) {
            log::trace!("{}: Closing ({}), {:?}", self.tag(), self.fd, self.fd);
            self.flags.insert(Flags::CLOSED);
            self.context.stopped(error);

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

impl StreamCtl {
    pub(crate) fn close(self) -> impl Future<Output = io::Result<()>> {
        let id = self.id as usize;
        let fut = self
            .inner
            .with(|streams| streams[id].close(self.id, &self.inner.api, None, true));
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
            let item = &mut streams[self.id as usize];
            if item.flags.intersects(Flags::CLOSED) {
                return false;
            }

            log::trace!(
                "{}: Modify interest ({:?}) rd: {:?}, wr: {:?}",
                item.tag(),
                item.fd,
                rd,
                wr
            );

            let mut flags = item.flags;
            let mut event = Event::new(0, false, false);

            if rd {
                if flags.contains(Flags::RD) {
                    event.readable = true;
                } else if item.read(&mut flags, self.id, &self.inner.api).is_pending() {
                    event.readable = true;
                    flags.insert(Flags::RD);
                }
            } else if flags.contains(Flags::RD) {
                flags.remove(Flags::RD);
            }

            if wr {
                if flags.contains(Flags::WR) {
                    event.writable = true;
                } else {
                    let result = item.context.with_write_buf(|buf| {
                        log::trace!(
                            "{}: Writing ({}), buf: {:?}",
                            item.tag(),
                            item.fd,
                            buf.len()
                        );
                        syscall!(
                            break libc::write(item.fd, buf[..].as_ptr() as _, buf.len())
                        )
                    });

                    if result.is_pending() {
                        event.writable = true;
                        flags.insert(Flags::WR);
                    }
                }
            } else if flags.contains(Flags::WR) {
                flags.remove(Flags::WR);
            }
            item.flags = flags;

            if !flags.contains(Flags::CLOSED) {
                self.inner.api.modify(item.fd, self.id, event);
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
                let mut item = streams.remove(id);
                log::trace!(
                    "{}: Drop io ({:?}), flags: {:?}",
                    item.tag(),
                    item.fd,
                    item.flags
                );
                item.close(self.id, &self.inner.api, None, true);
            }
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayd_drop.set(true);
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}
