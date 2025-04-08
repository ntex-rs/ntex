use std::{cell::Cell, cell::RefCell, io, mem, os, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::IoContext;
use ntex_neon::driver::{DriverApi, Event, Handler};
use ntex_neon::{syscall, Runtime};
use slab::Slab;
use socket2::Socket;

pub(crate) struct StreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const RD     = 0b0000_0001;
        const WR     = 0b0000_0010;
    }
}

struct StreamItem {
    io: Socket,
    flags: Flags,
    ref_count: u8,
    context: Option<IoContext>,
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
    /// Get `StreamOps` instance from the current runtime, or create new one
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

    /// Number of active streams
    pub(crate) fn active_ops() -> usize {
        Self::current().0.with(|streams| streams.len())
    }

    /// Register new stream
    pub(crate) fn register(&self, io: Socket, context: IoContext) -> StreamCtl {
        let fd = io.as_raw_fd();
        let stream = self.0.with(move |streams| {
            let item = StreamItem {
                io,
                ref_count: 1,
                flags: Flags::empty(),
                context: Some(context),
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
            let mut renew = Event::new(0, false, false);

            log::trace!("{}: Event ({:?}): {ev:?}", io.tag(), io.fd());

            if ev.readable {
                if io.read(id as u32, &self.inner.api) && io.can_read() {
                    renew.readable = true;
                    io.flags.insert(Flags::RD);
                } else {
                    io.flags.remove(Flags::RD);
                }
            } else if io.flags.contains(Flags::RD) {
                renew.readable = true;
            }

            if ev.writable {
                if io.write(id as u32, &self.inner.api) {
                    renew.writable = true;
                    io.flags.insert(Flags::WR);
                } else {
                    io.flags.remove(Flags::WR);
                }
            } else if io.flags.contains(Flags::WR) {
                renew.writable = true;
            }

            if ev.is_interrupt() {
                io.context.as_ref().inspect(|ctx| ctx.stopped(None));
            } else {
                self.inner.api.modify(io.fd(), id as u32, renew);
            }

            // delayed drops
            if self.inner.delayd_drop.get() {
                self.inner.delayd_drop.set(false);
                for id in self.inner.feed.borrow_mut().drain(..) {
                    let io = &mut streams[id as usize];
                    io.ref_count -= 1;
                    if io.ref_count == 0 {
                        close(streams, id, &self.inner.api);
                    }
                }
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(io) = streams.get_mut(id) {
                log::trace!("{}: Failed ({:?}) err: {err:?}", io.tag(), io.fd());
                if let Some(ref ctx) = io.context {
                    if !ctx.is_stopped() {
                        ctx.stopped(Some(err));
                    }
                }
                close(streams, id as u32, &self.inner.api);
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
    pub(crate) fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|streams| f(&streams[self.id as usize].io))
    }

    pub(crate) async fn shutdown(self) -> io::Result<()> {
        self.inner
            .with(|streams| {
                let item = &mut streams[self.id as usize];
                let fut = item.context.take().map(|ctx| {
                    let fd = item.fd();
                    ntex_rt::spawn_blocking(move || {
                        syscall!(libc::shutdown(fd, libc::SHUT_RDWR)).map(|_| ())
                    })
                });

                async move {
                    if let Some(fut) = fut {
                        fut.await
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
                            .and_then(crate::helpers::pool_io_err)
                    } else {
                        Ok(())
                    }
                }
            })
            .await
    }

    /// Modify poll interest for the stream
    pub(crate) fn interest(&self, rd: bool, wr: bool) {
        self.inner.with(|streams| {
            let io = &mut streams[self.id as usize];
            let mut event = Event::new(0, false, false);
            log::trace!("{}: Mod ({:?}) rd: {rd:?}, wr: {wr:?}", io.tag(), io.fd());

            if rd {
                if io.flags.contains(Flags::RD) {
                    event.readable = true;
                } else if io.read(self.id, &self.inner.api) {
                    event.readable = true;
                    io.flags.insert(Flags::RD);
                }
            } else {
                io.flags.remove(Flags::RD);
            }

            if wr {
                if io.flags.contains(Flags::WR) {
                    event.writable = true;
                } else if io.write(self.id, &self.inner.api) {
                    event.writable = true;
                    io.flags.insert(Flags::WR);
                }
            } else {
                io.flags.remove(Flags::WR);
            }

            self.inner.api.modify(io.fd(), self.id, event);
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
        // It is possible that drop happens while `StreamOps` handling event
        if let Some(mut streams) = self.inner.streams.take() {
            let id = self.id as usize;
            streams[id].ref_count -= 1;
            if streams[id].ref_count == 0 {
                close(&mut streams, self.id, &self.inner.api);
            }
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayd_drop.set(true);
            self.inner.feed.borrow_mut().push(self.id);
        }
    }
}

impl StreamItem {
    fn fd(&self) -> os::fd::RawFd {
        self.io.as_raw_fd()
    }

    fn tag(&self) -> &'static str {
        self.context
            .as_ref()
            .map(|ctx| ctx.tag())
            .unwrap_or_default()
    }

    fn can_read(&self) -> bool {
        self.context
            .as_ref()
            .map(|ctx| ctx.is_read_ready())
            .unwrap_or_default()
    }

    fn write(&mut self, id: u32, api: &DriverApi) -> bool {
        if let Some(ref ctx) = self.context {
            if let Some(buf) = ctx.get_write_buf() {
                let fd = self.fd();
                log::trace!("{}: Write ({fd:?}), buf: {:?}", ctx.tag(), buf.len());
                let res = syscall!(break libc::write(fd, buf[..].as_ptr() as _, buf.len()));
                return ctx.release_write_buf(buf, res);
            }
        }
        false
    }

    fn read(&mut self, id: u32, api: &DriverApi) -> bool {
        if let Some(ref ctx) = self.context {
            let (mut buf, hw, lw) = ctx.get_read_buf();

            let fd = self.fd();
            let mut total = 0;
            loop {
                // make sure we've got room
                if buf.remaining_mut() < lw {
                    buf.reserve(hw);
                }
                let chunk = buf.chunk_mut();
                let chunk_len = chunk.len();
                let chunk_ptr = chunk.as_mut_ptr();

                let res = syscall!(break libc::read(fd, chunk_ptr as _, chunk_len));
                if let Poll::Ready(Ok(size)) = res {
                    unsafe { buf.advance_mut(size) };
                    total += size;
                    if size == chunk_len {
                        continue;
                    }
                }

                log::trace!("{}: Read ({fd:?}), s: {total:?}, res: {res:?}", self.tag());
                return ctx.release_read_buf(total, buf, res.map(|res| res.map(|_| ())));
            }
        } else {
            false
        }
    }
}

fn close(st: &mut Slab<StreamItem>, id: u32, api: &DriverApi) {
    let mut item = st.remove(id as usize);
    let fd = item.fd();
    let shutdown = item
        .context
        .take()
        .map(|ctx| {
            if !ctx.is_stopped() {
                ctx.stopped(None);
            }
            true
        })
        .unwrap_or_default();

    log::trace!("{}: Close ({fd:?}), flags: {:?}", item.tag(), item.flags);

    api.detach(fd, id);
    mem::forget(item.io);
    ntex_rt::spawn_blocking(move || {
        if shutdown {
            let _ = syscall!(libc::shutdown(fd, libc::SHUT_RDWR));
        }
        syscall!(libc::close(fd)).inspect_err(|err| {
            log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
        })
    });
}
