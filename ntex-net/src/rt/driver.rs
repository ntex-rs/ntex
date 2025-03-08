use std::{cell::Cell, collections::VecDeque, fmt, io, ptr, rc::Rc, task, task::Poll};

use ntex_iodriver::op::{Handler, Interest};
use ntex_iodriver::{syscall, AsRawFd, DriverApi, RawFd};
use ntex_runtime::Runtime;
use slab::Slab;

use ntex_bytes::BufMut;
use ntex_io::{ReadContext, WriteContext};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const ERROR = 0b0000_0001;
        const RD    = 0b0000_0010;
        const WR    = 0b0000_0100;
    }
}

pub(crate) struct StreamCtl<T> {
    id: usize,
    inner: Rc<CompioOpsInner<T>>,
}

struct TcpStreamItem<T> {
    io: Option<T>,
    fd: RawFd,
    read: ReadContext,
    write: WriteContext,
    flags: Flags,
    ref_count: usize,
}

pub(crate) struct CompioOps<T>(Rc<CompioOpsInner<T>>);

#[derive(Debug)]
enum Change {
    Readable,
    Writable,
    Error(io::Error),
}

struct CompioOpsBatcher<T> {
    feed: VecDeque<(usize, Change)>,
    inner: Rc<CompioOpsInner<T>>,
}

struct CompioOpsInner<T> {
    api: DriverApi,
    feed: Cell<Option<VecDeque<usize>>>,
    streams: Cell<Option<Box<Slab<TcpStreamItem<T>>>>>,
}

impl<T: AsRawFd + 'static> CompioOps<T> {
    pub(crate) fn current() -> Self {
        Runtime::with_current(|rt| {
            if let Some(s) = rt.get::<Self>() {
                s
            } else {
                let mut inner = None;
                rt.driver().register_handler(|api| {
                    let ops = Rc::new(CompioOpsInner {
                        api,
                        feed: Cell::new(Some(VecDeque::new())),
                        streams: Cell::new(Some(Box::new(Slab::new()))),
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
        io: T,
        read: ReadContext,
        write: WriteContext,
    ) -> StreamCtl<T> {
        let item = TcpStreamItem {
            read,
            write,
            fd: io.as_raw_fd(),
            io: Some(io),
            flags: Flags::empty(),
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
        F: FnOnce(&mut Slab<TcpStreamItem<T>>) -> R,
    {
        let mut inner = self.0.streams.take().unwrap();
        let result = f(&mut inner);
        self.0.streams.set(Some(inner));
        result
    }
}

impl<T> Clone for CompioOps<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Handler for CompioOpsBatcher<T> {
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
                    let result = item.read.with_buf(|buf| {
                        let chunk = buf.chunk_mut();
                        let b = chunk.as_mut_ptr();
                        Poll::Ready(
                            task::ready!(syscall!(
                                break libc::read(item.fd, b as _, chunk.len())
                            ))
                            .inspect(|size| {
                                unsafe { buf.advance_mut(*size) };
                                log::debug!("FD: {:?}, BUF: {:?}", item.fd, buf);
                            }),
                        )
                    });

                    if result.is_pending() {
                        item.flags.insert(Flags::RD);
                        self.inner.api.register(item.fd, id, Interest::Readable);
                    }
                }
                Change::Writable => {
                    let item = &mut streams[id];
                    let result = item.write.with_buf(|buf| {
                        let slice = &buf[..];
                        syscall!(
                            break libc::write(item.fd, slice.as_ptr() as _, slice.len())
                        )
                    });

                    if result.is_pending() {
                        item.flags.insert(Flags::WR);
                        self.inner.api.register(item.fd, id, Interest::Writable);
                    }
                }
                Change::Error(err) => {
                    if let Some(item) = streams.get_mut(id) {
                        item.read.set_stopped(Some(err));
                        if !item.flags.contains(Flags::ERROR) {
                            item.flags.insert(Flags::ERROR);
                            item.flags.remove(Flags::RD | Flags::WR);
                            self.inner.api.unregister_all(item.fd);
                        }
                    }
                }
            }
        }

        // extra
        let mut feed = self.inner.feed.take().unwrap();
        for id in feed.drain(..) {
            log::debug!("Drop io ({}), {:?}", id, streams[id].fd);

            streams[id].ref_count -= 1;
            if streams[id].ref_count == 0 {
                let item = streams.remove(id);
                if item.io.is_some() {
                    self.inner.api.unregister_all(item.fd);
                }
            }
        }

        self.inner.feed.set(Some(feed));
        self.inner.streams.set(Some(streams));
    }
}

impl<T> StreamCtl<T> {
    pub(crate) fn take_io(&self) -> Option<T> {
        self.with(|streams| streams[self.id].io.take())
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

            if item.flags.intersects(Flags::RD | Flags::WR) {
                log::debug!("Pause all io ({}), {:?}", self.id, item.fd);
                item.flags.remove(Flags::RD | Flags::WR);
                self.inner.api.unregister_all(item.fd);
            }
        })
    }

    pub(crate) fn pause_read(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            log::debug!("Pause io read ({}), {:?}", self.id, item.fd);
            if item.flags.contains(Flags::RD) {
                item.flags.remove(Flags::RD);
                self.inner.api.unregister(item.fd, Interest::Readable);
            }
        })
    }

    pub(crate) fn resume_read(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            log::debug!("Resume io read ({}), {:?}", self.id, item.fd);
            if !item.flags.contains(Flags::RD) {
                item.flags.insert(Flags::RD);
                self.inner
                    .api
                    .register(item.fd, self.id, Interest::Readable);
            }
        })
    }

    pub(crate) fn resume_write(&self) {
        self.with(|streams| {
            let item = &mut streams[self.id];

            if !item.flags.contains(Flags::WR) {
                log::debug!("Resume io write ({}), {:?}", self.id, item.fd);
                let result = item.write.with_buf(|buf| {
                    log::debug!("Writing io ({}), buf: {:?}", self.id, buf.len());

                    let slice = &buf[..];
                    syscall!(break libc::write(item.fd, slice.as_ptr() as _, slice.len()))
                });

                if result.is_pending() {
                    log::debug!("Write is pending ({}), {:?}", self.id, item.read.flags());

                    item.flags.insert(Flags::WR);
                    self.inner
                        .api
                        .register(item.fd, self.id, Interest::Writable);
                }
            }
        })
    }

    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Slab<TcpStreamItem<T>>) -> R,
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
            log::debug!("Drop io ({}), {:?}", self.id, streams[self.id].fd);

            streams[self.id].ref_count -= 1;
            if streams[self.id].ref_count == 0 {
                let item = streams.remove(self.id);
                if item.io.is_some() {
                    self.inner.api.unregister_all(item.fd);
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

impl<T> PartialEq for StreamCtl<T> {
    #[inline]
    fn eq(&self, other: &StreamCtl<T>) -> bool {
        self.id == other.id && ptr::eq(&self.inner, &other.inner)
    }
}

impl<T: fmt::Debug> fmt::Debug for StreamCtl<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with(|streams| {
            f.debug_struct("StreamCtl")
                .field("id", &self.id)
                .field("io", &streams[self.id].io)
                .finish()
        })
    }
}
