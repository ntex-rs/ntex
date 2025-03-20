use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, collections::VecDeque, future::Future, io, rc::Rc, task};

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
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
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

#[derive(Debug)]
enum Change {
    Event(Event),
    Error(io::Error),
}

struct StreamOpsHandler<T> {
    feed: VecDeque<(usize, Change)>,
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamOpsInner<T> {
    api: DriverApi,
    feed: Cell<Option<VecDeque<u32>>>,
    streams: Cell<Option<Box<Slab<StreamItem<T>>>>>,
}

impl<T: AsRawFd + 'static> StreamOps<T> {
    pub(crate) fn current() -> Self {
        Runtime::value(|rt| {
            let mut inner = None;
            rt.driver().register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    feed: Cell::new(Some(VecDeque::new())),
                    streams: Cell::new(Some(Box::new(Slab::new()))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler {
                    inner: ops,
                    feed: VecDeque::new(),
                })
            });

            StreamOps(inner.unwrap())
        })
    }

    pub(crate) fn register(&self, io: T, context: IoContext) -> StreamCtl<T> {
        let fd = io.as_raw_fd();
        let item = StreamItem {
            fd,
            context,
            io: Some(io),
            ref_count: 1,
            flags: Flags::empty(),
        };
        let stream = self.with(move |streams| {
            let id = streams.insert(item) as u32;
            StreamCtl {
                id,
                inner: self.0.clone(),
            }
        });

        self.0.api.attach(fd, stream.id, None);
        stream
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<StreamItem<T>>) -> R,
    {
        let mut inner = self.0.streams.take().unwrap();
        let result = f(&mut inner);
        self.0.streams.set(Some(inner));
        result
    }
}

impl<T> Clone for StreamOps<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Handler for StreamOpsHandler<T> {
    fn event(&mut self, id: usize, event: Event) {
        log::debug!("FD is readable {:?}", id);
        self.feed.push_back((id, Change::Event(event)));
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

        let mut streams = self.inner.streams.take().unwrap();

        for (id, change) in self.feed.drain(..) {
            match change {
                Change::Event(ev) => {
                    let item = &mut streams[id];
                    let mut renew_ev = Event::new(0, false, false).with_interrupt();
                    if ev.readable {
                        let result = item.context.with_read_buf(|buf| {
                            let chunk = buf.chunk_mut();
                            let b = chunk.as_mut_ptr();
                            task::Poll::Ready(
                                task::ready!(syscall!(
                                    break libc::read(item.fd, b as _, chunk.len())
                                ))
                                .inspect(|size| {
                                    unsafe { buf.advance_mut(*size) };
                                    log::debug!(
                                        "{}: {:?}, SIZE: {:?}",
                                        item.context.tag(),
                                        item.fd,
                                        size
                                    );
                                }),
                            )
                        });

                        if item.io.is_some() && result.is_pending() {
                            if item.context.is_read_ready() {
                                renew_ev.readable = true;
                            }
                        }
                    } else if item.flags.contains(Flags::RD) {
                        renew_ev.readable = true;
                    }

                    if ev.writable {
                        let result = item.context.with_write_buf(|buf| {
                            log::debug!(
                                "{}: writing {:?} SIZE: {:?}",
                                item.context.tag(),
                                item.fd,
                                buf.len()
                            );
                            let slice = &buf[..];
                            syscall!(
                                break libc::write(
                                    item.fd,
                                    slice.as_ptr() as _,
                                    slice.len()
                                )
                            )
                        });

                        if item.io.is_some() && result.is_pending() {
                            renew_ev.writable = true;
                        }
                    } else if item.flags.contains(Flags::WR) {
                        renew_ev.writable = true;
                    }

                    if ev.is_interrupt() {
                        item.context.stopped(None);
                        if let Some(_) = item.io.take() {
                            close(id as u32, item.fd, &self.inner.api);
                        }
                        continue;
                    } else {
                        item.flags.set(Flags::RD, renew_ev.readable);
                        item.flags.set(Flags::WR, renew_ev.writable);
                        self.inner.api.modify(item.fd, id as u32, renew_ev);
                    }
                }
                Change::Error(err) => {
                    if let Some(item) = streams.get_mut(id) {
                        item.context.stopped(Some(err));
                        if let Some(_) = item.io.take() {
                            close(id as u32, item.fd, &self.inner.api);
                        }
                    }
                }
            }
        }

        // extra
        let mut feed = self.inner.feed.take().unwrap();
        for id in feed.drain(..) {
            let item = &mut streams[id as usize];
            item.ref_count -= 1;
            if item.ref_count == 0 {
                let item = streams.remove(id as usize);
                log::debug!(
                    "{}: Drop io ({}), {:?}, has-io: {}",
                    item.context.tag(),
                    id,
                    item.fd,
                    item.io.is_some()
                );
                if item.io.is_some() {
                    close(id, item.fd, &self.inner.api);
                }
            }
        }

        self.inner.feed.set(Some(feed));
        self.inner.streams.set(Some(streams));
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
        let (io, fd) = self.with(|streams| (streams[id].io.take(), streams[id].fd));
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
        self.with(|streams| f(streams[self.id as usize].io.as_ref()))
    }

    pub(crate) fn modify(&self, readable: bool, writable: bool) {
        self.with(|streams| {
            let item = &mut streams[self.id as usize];
            item.flags = Flags::empty();

            log::debug!(
                "{}: Modify interest ({}), {:?} read: {:?}, write: {:?}",
                item.context.tag(),
                self.id,
                item.fd,
                readable,
                writable
            );

            let mut event = Event::new(0, false, false).with_interrupt();

            if readable {
                let result = item.context.with_read_buf(|buf| {
                    let chunk = buf.chunk_mut();
                    let b = chunk.as_mut_ptr();
                    task::Poll::Ready(
                        task::ready!(syscall!(
                            break libc::read(item.fd, b as _, chunk.len())
                        ))
                        .inspect(|size| {
                            unsafe { buf.advance_mut(*size) };
                            log::debug!(
                                "{}: {:?}, SIZE: {:?}",
                                item.context.tag(),
                                item.fd,
                                size
                            );
                        }),
                    )
                });

                if item.io.is_some() && result.is_pending() {
                    if item.context.is_read_ready() {
                        event.readable = true;
                        item.flags.insert(Flags::RD);
                    }
                }
            }

            if writable {
                let result = item.context.with_write_buf(|buf| {
                    log::debug!(
                        "{}: Writing io ({}), buf: {:?}",
                        item.context.tag(),
                        self.id,
                        buf.len()
                    );

                    let slice = &buf[..];
                    syscall!(break libc::write(item.fd, slice.as_ptr() as _, slice.len()))
                });

                if item.io.is_some() && result.is_pending() {
                    event.writable = true;
                    item.flags.insert(Flags::WR);
                }
            }

            self.inner.api.modify(item.fd, self.id as u32, event);
        })
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<StreamItem<T>>) -> R,
    {
        let mut inner = self.inner.streams.take().unwrap();
        let result = f(&mut inner);
        self.inner.streams.set(Some(inner));
        result
    }
}

impl<T> Clone for StreamCtl<T> {
    fn clone(&self) -> Self {
        self.with(|streams| {
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
                    item.context.tag(),
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
            let mut feed = self.inner.feed.take().unwrap();
            feed.push_back(self.id);
            self.inner.feed.set(Some(feed));
        }
    }
}
