use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, collections::VecDeque, future::Future, io, rc::Rc, task};

use ntex_neon::driver::{DriverApi, Handler, Interest};
use ntex_neon::{syscall, Runtime};
use slab::Slab;

use ntex_bytes::BufMut;
use ntex_io::IoContext;

pub(crate) struct StreamCtl<T> {
    id: usize,
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamItem<T> {
    io: Option<T>,
    fd: RawFd,
    context: IoContext,
    ref_count: usize,
}

pub(crate) struct StreamOps<T>(Rc<StreamOpsInner<T>>);

#[derive(Debug)]
enum Change {
    Readable,
    Writable,
    Error(io::Error),
}

struct StreamOpsHandler<T> {
    feed: VecDeque<(usize, Change)>,
    inner: Rc<StreamOpsInner<T>>,
}

struct StreamOpsInner<T> {
    api: DriverApi,
    feed: Cell<Option<VecDeque<usize>>>,
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
        let item = StreamItem {
            context,
            fd: io.as_raw_fd(),
            io: Some(io),
            ref_count: 1,
        };
        self.with(|streams| {
            let id = streams.insert(item);
            StreamCtl {
                id,
                inner: self.0.clone(),
            }
        })
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

        let mut streams = self.inner.streams.take().unwrap();

        for (id, change) in self.feed.drain(..) {
            match change {
                Change::Readable => {
                    let item = &mut streams[id];
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
                                    "{}: {:?}, SIZE: {:?}, BUF: {:?}",
                                    item.context.tag(),
                                    item.fd,
                                    size,
                                    buf,
                                );
                            }),
                        )
                    });

                    if item.io.is_some() && result.is_pending() {
                        self.inner.api.register(item.fd, id, Interest::Readable);
                    }
                }
                Change::Writable => {
                    let item = &mut streams[id];
                    let result = item.context.with_write_buf(|buf| {
                        log::debug!(
                            "{}: writing {:?} SIZE: {:?}, BUF: {:?}",
                            item.context.tag(),
                            item.fd,
                            buf.len(),
                            buf,
                        );
                        let slice = &buf[..];
                        syscall!(
                            break libc::write(item.fd, slice.as_ptr() as _, slice.len())
                        )
                    });

                    if item.io.is_some() && result.is_pending() {
                        log::debug!(
                            "{}: want write {:?}",
                            item.context.tag(),
                            item.fd,
                        );
                        self.inner.api.register(item.fd, id, Interest::Writable);
                    }
                }
                Change::Error(err) => {
                    if let Some(item) = streams.get_mut(id) {
                        item.context.stopped(Some(err));
                        if let Some(_) = item.io.take() {
                            close(id, item.fd, &self.inner.api);
                        }
                    }
                }
            }
        }

        // extra
        let mut feed = self.inner.feed.take().unwrap();
        for id in feed.drain(..) {
            let item = &mut streams[id];
            item.ref_count -= 1;
            if item.ref_count == 0 {
                let item = streams.remove(id);
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

fn close(id: usize, fd: RawFd, api: &DriverApi) -> ntex_rt::JoinHandle<io::Result<i32>> {
    api.unregister_all(fd);
    ntex_rt::spawn_blocking(move || {
        syscall!(libc::shutdown(fd, libc::SHUT_RDWR))?;
        syscall!(libc::close(fd))
    })
}

impl<T> StreamCtl<T> {
    pub(crate) fn close(self) -> impl Future<Output = io::Result<()>> {
        let (context, io, fd) =
            self.with(|streams| (streams[self.id].context.clone(), streams[self.id].io.take(), streams[self.id].fd));
        let fut = if let Some(io) = io {
            log::debug!("{}: Closing ({}), {:?}", context.tag(), self.id, fd);
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
        self.with(|streams| f(streams[self.id].io.as_ref()))
    }

    pub(crate) fn pause_all(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            log::debug!(
                "{}: Pause all io ({}), {:?}",
                item.context.tag(),
                self.id,
                item.fd
            );
            self.inner.api.unregister_all(item.fd);
        })
    }

    pub(crate) fn pause_read(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            log::debug!(
                "{}: Pause io read ({}), {:?}",
                item.context.tag(),
                self.id,
                item.fd
            );
            self.inner.api.unregister(item.fd, Interest::Readable);
        })
    }

    pub(crate) fn resume_read(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            log::debug!(
                "{}: Resume io read ({}), {:?}",
                item.context.tag(),
                self.id,
                item.fd
            );

            let result = item.context.with_read_buf(|buf| {
                let chunk = buf.chunk_mut();
                let b = chunk.as_mut_ptr();
                task::Poll::Ready(
                    task::ready!(syscall!(break libc::read(item.fd, b as _, chunk.len())))
                        .inspect(|size| {
                            unsafe { buf.advance_mut(*size) };
                            log::debug!(
                                "{}: {:?}, SIZE: {:?}, BUF: {:?}",
                                item.context.tag(),
                                item.fd,
                                size,
                                buf,
                            );
                        }),
                )
            });

            if item.io.is_some() && result.is_pending() {
                self.inner
                    .api
                    .register(item.fd, self.id, Interest::Readable);
            }
        })
    }

    pub(crate) fn resume_write(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

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
                log::debug!(
                    "{}: Write is pending ({}), {:?}",
                    item.context.tag(),
                    self.id,
                    item.context.flags()
                );

                self.inner
                    .api
                    .register(item.fd, self.id, Interest::Writable);
            }
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
            streams[self.id].ref_count += 1;
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
            streams[self.id].ref_count -= 1;
            if streams[self.id].ref_count == 0 {
                let item = streams.remove(self.id);
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
