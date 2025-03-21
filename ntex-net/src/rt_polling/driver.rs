use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, cell::RefCell, future::Future, io, rc::Rc, task, task::Poll};

use ntex_neon::driver::{DriverApi, Event, Handler};
use ntex_neon::{syscall, Runtime};
use slab::Slab;

use ntex_bytes::BufMut;
use ntex_io::IoContext;

pub(crate) struct StreamCtl<T> {
    id: u32,
    inner: Rc<StreamOpsInner<T>>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const RD = 0b0000_0001;
        const WR = 0b0000_0010;
    }
}

struct StreamItem<T> {
    io: Option<T>,
    fd: RawFd,
    flags: Flags,
    ref_count: u16,
    context: IoContext,
}

pub(crate) struct StreamOps<T>(Rc<StreamOpsInner<T>>);

struct StreamOpsHandler<T> {
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamOpsInner<T> {
    api: DriverApi,
    delayd_drop: Cell<bool>,
    feed: RefCell<Vec<u32>>,
    streams: Cell<Option<Box<Slab<StreamItem<T>>>>>,
}

impl<T> StreamItem<T> {
    fn tag(&self) -> &'static str {
        self.context.tag()
    }
}

impl<T: AsRawFd + 'static> StreamOps<T> {
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

    pub(crate) fn register(&self, io: T, context: IoContext) -> StreamCtl<T> {
        let fd = io.as_raw_fd();
        let stream = self.0.with(move |streams| {
            let item = StreamItem {
                fd,
                context,
                io: Some(io),
                ref_count: 1,
                flags: Flags::empty(),
            };
            StreamCtl {
                id: streams.insert(item) as u32,
                inner: self.0.clone(),
            }
        });

        self.0.api.attach(
            fd,
            stream.id,
            Some(Event::new(0, false, false).with_interrupt()),
        );
        stream
    }
}

impl<T> Clone for StreamOps<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Handler for StreamOpsHandler<T> {
    fn event(&mut self, id: usize, ev: Event) {
        log::debug!("FD event {:?} event: {:?}", id, ev);

        self.inner.with(|streams| {
            if !streams.contains(id) {
                return;
            }
            let item = &mut streams[id];

            // handle HUP
            if ev.is_interrupt() {
                item.context.stopped(None);
                if item.io.take().is_some() {
                    close(id as u32, item.fd, &self.inner.api);
                }
                return;
            }

            let mut renew_ev = Event::new(0, false, false).with_interrupt();

            if ev.readable {
                let res = item.context.with_read_buf(|buf| {
                    let chunk = buf.chunk_mut();
                    let result = task::ready!(syscall!(
                        break libc::read(item.fd, chunk.as_mut_ptr() as _, chunk.len())
                    ));
                    if let Ok(size) = result {
                        log::debug!("{}: data {:?}, s: {:?}", item.tag(), item.fd, size);
                        unsafe { buf.advance_mut(size) };
                    }
                    Poll::Ready(result)
                });

                if res.is_pending() && item.context.is_read_ready() {
                    renew_ev.readable = true;
                    item.flags.insert(Flags::RD);
                } else {
                    item.flags.remove(Flags::RD);
                }
            } else if item.flags.contains(Flags::RD) {
                renew_ev.readable = true;
            }

            if ev.writable {
                let result = item.context.with_write_buf(|buf| {
                    log::debug!("{}: write {:?} s: {:?}", item.tag(), item.fd, buf.len());
                    syscall!(break libc::write(item.fd, buf[..].as_ptr() as _, buf.len()))
                });
                if result.is_pending() {
                    renew_ev.writable = true;
                    item.flags.insert(Flags::WR);
                } else {
                    item.flags.remove(Flags::WR);
                }
            } else if item.flags.contains(Flags::WR) {
                renew_ev.writable = true;
            }

            self.inner.api.modify(item.fd, id as u32, renew_ev);

            // delayed drops
            if self.inner.delayd_drop.get() {
                for id in self.inner.feed.borrow_mut().drain(..) {
                    let item = &mut streams[id as usize];
                    item.ref_count -= 1;
                    if item.ref_count == 0 {
                        let item = streams.remove(id as usize);
                        log::debug!(
                            "{}: Drop ({}), {:?}, has-io: {}",
                            item.tag(),
                            id,
                            item.fd,
                            item.io.is_some()
                        );
                        if item.io.is_some() {
                            close(id, item.fd, &self.inner.api);
                        }
                    }
                }
                self.inner.delayd_drop.set(false);
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(item) = streams.get_mut(id) {
                log::debug!("FD is failed ({}) {:?}, err: {:?}", id, item.fd, err);
                item.context.stopped(Some(err));
                if item.io.take().is_some() {
                    close(id as u32, item.fd, &self.inner.api);
                }
            }
        })
    }
}

impl<T> StreamOpsInner<T> {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<StreamItem<T>>) -> R,
    {
        let mut streams = self.streams.take().unwrap();
        let result = f(&mut streams);
        self.streams.set(Some(streams));
        result
    }
}

fn close(id: u32, fd: RawFd, api: &DriverApi) -> ntex_rt::JoinHandle<io::Result<i32>> {
    api.detach(fd, id);
    ntex_rt::spawn_blocking(move || {
        syscall!(libc::shutdown(fd, libc::SHUT_RDWR))?;
        syscall!(libc::close(fd))
    })
}

impl<T> StreamCtl<T> {
    pub(crate) fn close(self) -> impl Future<Output = io::Result<()>> {
        let id = self.id as usize;
        let (io, fd) = self
            .inner
            .with(|streams| (streams[id].io.take(), streams[id].fd));
        let fut = if let Some(io) = io {
            log::debug!("Closing ({}), {:?}", id, fd);
            std::mem::forget(io);
            Some(close(self.id, fd, &self.inner.api))
        } else {
            None
        };
        async move {
            if let Some(fut) = fut {
                fut.await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                    .and_then(crate::helpers::pool_io_err)?;
            }
            Ok(())
        }
    }

    pub(crate) fn with_io<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Option<&T>) -> R,
    {
        self.inner
            .with(|streams| f(streams[self.id as usize].io.as_ref()))
    }

    pub(crate) fn modify(&self, rd: bool, wr: bool) {
        self.inner.with(|streams| {
            let item = &mut streams[self.id as usize];

            log::debug!(
                "{}: Modify interest ({}), {:?} rd: {:?}, wr: {:?}",
                item.tag(),
                self.id,
                item.fd,
                rd,
                wr
            );

            let mut event = Event::new(0, false, false).with_interrupt();

            if rd {
                if item.flags.contains(Flags::RD) {
                    event.readable = true;
                } else {
                    let res = item.context.with_read_buf(|buf| {
                        let chunk = buf.chunk_mut();
                        let result = task::ready!(syscall!(
                            break libc::read(item.fd, chunk.as_mut_ptr() as _, chunk.len())
                        ));
                        if let Ok(size) = result {
                            log::debug!(
                                "{}: read {:?}, s: {:?}",
                                item.tag(),
                                item.fd,
                                size
                            );
                            unsafe { buf.advance_mut(size) };
                        }
                        Poll::Ready(result)
                    });

                    if res.is_pending() && item.context.is_read_ready() {
                        event.readable = true;
                        item.flags.insert(Flags::RD);
                    }
                }
            }

            if wr {
                if item.flags.contains(Flags::WR) {
                    event.writable = true;
                } else {
                    let result = item.context.with_write_buf(|buf| {
                        log::debug!(
                            "{}: Writing ({}), buf: {:?}",
                            item.tag(),
                            self.id,
                            buf.len()
                        );
                        syscall!(
                            break libc::write(item.fd, buf[..].as_ptr() as _, buf.len())
                        )
                    });

                    if result.is_pending() {
                        event.writable = true;
                        item.flags.insert(Flags::WR);
                    }
                }
            }

            self.inner.api.modify(item.fd, self.id, event);
        })
    }
}

impl<T> Clone for StreamCtl<T> {
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

impl<T> Drop for StreamCtl<T> {
    fn drop(&mut self) {
        if let Some(mut streams) = self.inner.streams.take() {
            let id = self.id as usize;
            streams[id].ref_count -= 1;
            if streams[id].ref_count == 0 {
                let item = streams.remove(id);
                log::debug!(
                    "{}:  Drop io ({}), {:?}, has-io: {}",
                    item.tag(),
                    self.id,
                    item.fd,
                    item.io.is_some()
                );
                if item.io.is_some() {
                    close(self.id, item.fd, &self.inner.api);
                }
            }
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayd_drop.set(true);
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}
