use std::os::fd::{AsRawFd, RawFd};
use std::{cell::Cell, cell::RefCell, future::Future, io, mem, rc::Rc, task::Poll};

use ntex_neon::driver::{DriverApi, Event, Handler, PollMode};
use ntex_neon::{syscall, Runtime};
use slab::Slab;

use ntex_bytes::{BufMut, BytesVec};
use ntex_io::IoContext;

pub(crate) struct StreamCtl<T> {
    id: u32,
    inner: Rc<StreamOpsInner<T>>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const RD   = 0b0000_0001;
        const WR   = 0b0000_0010;
        const ERR  = 0b0000_0100;
        const RDSH = 0b0000_1000;
    }
}

struct StreamItem<T> {
    io: Option<T>,
    fd: RawFd,
    flags: Cell<Flags>,
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

    fn contains(&self, flag: Flags) -> bool {
        self.flags.get().contains(flag)
    }

    fn insert(&self, fl: Flags) {
        let mut flags = self.flags.get();
        flags.insert(fl);
        self.flags.set(flags);
    }

    fn remove(&self, fl: Flags) {
        let mut flags = self.flags.get();
        flags.remove(fl);
        self.flags.set(flags);
    }
}

impl<T: AsRawFd + 'static> StreamOps<T> {
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

    pub(crate) fn register(&self, io: T, context: IoContext) -> StreamCtl<T> {
        let fd = io.as_raw_fd();
        let stream = self.0.with(move |streams| {
            let item = StreamItem {
                fd,
                context,
                io: Some(io),
                ref_count: 1,
                flags: Cell::new(Flags::empty()),
            };
            StreamCtl {
                id: streams.insert(item) as u32,
                inner: self.0.clone(),
            }
        });

        self.0.api.attach(
            fd,
            stream.id,
            Event::new(0, false, false).with_interrupt(),
            PollMode::Edge,
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
        self.inner.with(|streams| {
            if !streams.contains(id) {
                return;
            }
            let item = &mut streams[id];
            if item.io.is_none() {
                return;
            }
            log::debug!("{}: FD event {:?} event: {:?}", item.tag(), id, ev);

            let mut changed = false;
            let mut renew_ev = Event::new(0, false, false).with_interrupt();

            // handle read op
            if ev.readable {
                let res = item
                    .context
                    .with_read_buf(|buf, hw, lw| read(item, buf, hw, lw));

                if res.is_pending() && item.context.is_read_ready() {
                    renew_ev.readable = true;
                } else {
                    changed = true;
                    item.remove(Flags::RD);
                }
            } else if item.contains(Flags::RD) {
                renew_ev.readable = true;
            }

            // handle error
            if ev.is_err() == Some(true) {
                item.insert(Flags::ERR);
            }

            // handle HUP
            if ev.is_interrupt() {
                item.context.stopped(None);
                close(id as u32, item, &self.inner.api, None);
                return;
            }

            // handle write op
            if ev.writable {
                let result = item.context.with_write_buf(|buf| {
                    log::debug!("{}: write {:?} s: {:?}", item.tag(), item.fd, buf.len());
                    syscall!(break libc::write(item.fd, buf[..].as_ptr() as _, buf.len()))
                });
                if result.is_pending() {
                    renew_ev.writable = true;
                } else {
                    changed = true;
                    item.remove(Flags::WR);
                }
            } else if item.contains(Flags::WR) {
                renew_ev.writable = true;
            }

            if changed {
                self.inner
                    .api
                    .modify(item.fd, id as u32, renew_ev, PollMode::Edge);
            }

            // delayed drops
            if self.inner.delayd_drop.get() {
                for id in self.inner.feed.borrow_mut().drain(..) {
                    let item = &mut streams[id as usize];
                    item.ref_count -= 1;
                    if item.ref_count == 0 {
                        let mut item = streams.remove(id as usize);
                        log::debug!(
                            "{}: Drop ({}), {:?}, has-io: {}",
                            item.tag(),
                            id,
                            item.fd,
                            item.io.is_some()
                        );
                        close(id, &mut item, &self.inner.api, None);
                    }
                }
                self.inner.delayd_drop.set(false);
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(item) = streams.get_mut(id) {
                log::debug!(
                    "{}: FD is failed ({}) {:?}, err: {:?}",
                    item.tag(),
                    id,
                    item.fd,
                    err
                );
                close(id as u32, item, &self.inner.api, Some(err));
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

fn read<T>(
    item: &StreamItem<T>,
    buf: &mut BytesVec,
    hw: usize,
    lw: usize,
) -> Poll<io::Result<usize>> {
    log::debug!(
        "{}: reading fd ({:?}) flags: {:?}",
        item.tag(),
        item.fd,
        item.context.flags()
    );
    if item.contains(Flags::RDSH) {
        return Poll::Ready(Ok(0));
    }

    let mut total = 0;
    loop {
        // make sure we've got room
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }

        let chunk = buf.chunk_mut();
        let chunk_len = chunk.len();

        let result =
            syscall!(break libc::read(item.fd, chunk.as_mut_ptr() as _, chunk.len()));
        if let Poll::Ready(Ok(size)) = result {
            unsafe { buf.advance_mut(size) };
            total += size;
            //if size != 0 {
            if size == chunk_len {
                continue;
            }
        }

        log::debug!(
            "{}: read fd ({:?}), s: {:?}, cap: {:?}, result: {:?}",
            item.tag(),
            item.fd,
            total,
            buf.remaining_mut(),
            result
        );

        return match result {
            Poll::Ready(Err(err)) => {
                if total > 0 {
                    item.insert(Flags::ERR);
                    item.context.stopped(Some(err));
                    Poll::Ready(Ok(total))
                } else {
                    Poll::Ready(Err(err))
                }
            }
            Poll::Ready(Ok(size)) => {
                if size == 0 {
                    item.insert(Flags::RDSH);
                    item.context.stopped(None);
                }
                Poll::Ready(Ok(total))
            }
            Poll::Pending => {
                if total > 0 {
                    Poll::Ready(Ok(total))
                } else {
                    Poll::Pending
                }
            }
        };
    }
}

fn close<T>(
    id: u32,
    item: &mut StreamItem<T>,
    api: &DriverApi,
    error: Option<io::Error>,
) -> Option<ntex_rt::JoinHandle<io::Result<i32>>> {
    if let Some(io) = item.io.take() {
        log::debug!("{}: Closing ({}), {:?}", item.tag(), id, item.fd);
        mem::forget(io);
        let shutdown = if let Some(err) = error {
            item.context.stopped(Some(err));
            false
        } else {
            !item.flags.get().intersects(Flags::ERR | Flags::RDSH)
        };
        let fd = item.fd;
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

impl<T> StreamCtl<T> {
    pub(crate) fn close(self) -> impl Future<Output = io::Result<()>> {
        let id = self.id as usize;
        let fut = self.inner.with(|streams| {
            let item = &mut streams[id];
            close(self.id, item, &self.inner.api, None)
        });
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
            if item.contains(Flags::ERR) {
                return;
            }

            log::debug!(
                "{}: Modify interest ({}), {:?} rd: {:?}, wr: {:?}, flags: {:?}",
                item.tag(),
                self.id,
                item.fd,
                rd,
                wr,
                item.flags
            );

            let mut changed = false;
            let mut event = Event::new(0, false, false).with_interrupt();

            if rd {
                if item.contains(Flags::RD) {
                    event.readable = true;
                } else {
                    let res = item
                        .context
                        .with_read_buf(|buf, hw, lw| read(item, buf, hw, lw));

                    if res.is_pending() && item.context.is_read_ready() {
                        changed = true;
                        event.readable = true;
                        item.insert(Flags::RD);
                    }
                }
            }

            if wr {
                if item.contains(Flags::WR) {
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
                        changed = true;
                        event.writable = true;
                        item.insert(Flags::WR);
                    }
                }
            }

            if changed {
                self.inner
                    .api
                    .modify(item.fd, self.id, event, PollMode::Edge);
            }
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
                let mut item = streams.remove(id);
                log::debug!(
                    "{}:  Drop io ({}), {:?}, has-io: {}",
                    item.tag(),
                    self.id,
                    item.fd,
                    item.io.is_some()
                );
                close(self.id, &mut item, &self.inner.api, None);
            }
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayd_drop.set(true);
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}
