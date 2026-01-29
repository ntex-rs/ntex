use std::{cell::Cell, io, mem, os, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::BufMut;
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::{Arbiter, syscall};
use slab::Slab;
use socket2::Socket;

use super::{Driver, DriverApi, Event, Handler};

pub(crate) struct StreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

pub(crate) struct WeakStreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const RD          = 0b0000_0001;
        const WR          = 0b0000_0010;
        const DROPPED_PRI = 0b0001_0000;
        const DROPPED_SEC = 0b0010_0000;
    }
}

enum IdType {
    Stream(u32),
    Weak(u32),
}

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    flags: Flags,
    ctx: IoContext,
}

pub(crate) struct StreamOps(Rc<StreamOpsInner>);

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

struct StreamOpsInner {
    api: DriverApi,
    delayed_drop: Cell<bool>,
    delayed_feed: Cell<Option<Vec<IdType>>>,
    streams: Cell<Option<Box<Slab<StreamItem>>>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Driver) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    delayed_drop: Cell::new(false),
                    delayed_feed: Cell::new(Some(Vec::new())),
                    streams: Cell::new(Some(Box::new(Slab::new()))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    /// Register new stream
    pub(crate) fn register(
        &self,
        io: Socket,
        ctx: IoContext,
    ) -> (StreamCtl, WeakStreamCtl) {
        let fd = io.as_raw_fd();
        let stream = self.0.with(move |streams| {
            let item = StreamItem {
                io,
                ctx,
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

        let weak = WeakStreamCtl {
            id: stream.id,
            inner: self.0.clone(),
        };

        (stream, weak)
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
            let mut renew = Event::new(0, false, false).with_interrupt();
            log::trace!(
                "{}: {:?}-Evt rd({:?}) wr({:?}) {:?}",
                io.tag(),
                io.fd(),
                ev.readable,
                ev.writable,
                io.flags
            );

            if ev.readable {
                if io.read().ready() {
                    renew.readable = true;
                    io.flags.insert(Flags::RD);
                } else {
                    io.flags.remove(Flags::RD);
                }
            } else if io.flags.contains(Flags::RD) {
                renew.readable = true;
            }

            if ev.writable {
                if io.write().ready() {
                    renew.writable = true;
                    io.flags.insert(Flags::WR);
                } else {
                    io.flags.remove(Flags::WR);
                }
            } else if io.flags.contains(Flags::WR) {
                renew.writable = true;
            }

            if ev.is_interrupt() {
                io.ctx.stop(None);
            } else {
                log::trace!(
                    "{}: {:?}-Renew rd({:?}) wr({:?})",
                    io.tag(),
                    io.fd(),
                    renew.readable,
                    renew.writable
                );
                self.inner.api.modify(io.fd(), id as u32, renew);
            }
        });
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(io) = streams.get_mut(id) {
                log::trace!("{}: {:?}-Failed err({err:?})", io.tag(), io.fd());
                if !io.ctx.is_stopped() {
                    io.ctx.stop(Some(err));
                }
            }
        });
    }

    fn tick(&mut self) {
        self.inner.check_delayed_feed();
    }

    fn cleanup(&mut self) {
        if let Some(v) = self.inner.streams.take() {
            for (_, val) in v.into_iter() {
                if val.flags.contains(Flags::DROPPED_PRI) {
                    mem::forget(val.io);
                } else {
                    log::trace!(
                        "{}: Unclosed sockets {:?}",
                        val.ctx.tag(),
                        val.io.peer_addr()
                    );
                }
            }
        }
        self.inner.delayed_feed.take();
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

    fn drop_stream(&self, id: u32) {
        // Dropping while `StreamOps` handling event
        if let Some(mut streams) = self.streams.take() {
            let idx = id as usize;
            let item = &mut streams[idx];
            let fd = item.fd();
            log::trace!("{}: {fd:?}-Close flags: {:?}", item.tag(), item.flags);

            if !item.ctx.is_stopped() {
                item.ctx.stop(None);
            }
            self.api.detach(fd, id);

            if item.flags.contains(Flags::DROPPED_SEC) {
                let item = streams.remove(idx);
                ntex_rt::spawn_blocking(move || {
                    if let Err(err) = syscall!(libc::close(fd)) {
                        log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
                    }
                });
                mem::forget(item.io);
            } else {
                item.flags.insert(Flags::DROPPED_PRI);
            }
            self.streams.set(Some(streams));
        } else {
            self.add_delayed_drop(IdType::Stream(id));
        }
    }

    fn drop_weak_stream(&self, id: u32) {
        // Dropping while `StreamOps` handling event
        if let Some(mut streams) = self.streams.take() {
            let idx = id as usize;
            let item = &mut streams[idx];

            if item.flags.contains(Flags::DROPPED_PRI) {
                let item = streams.remove(idx);
                let fd = item.fd();
                ntex_rt::spawn_blocking(move || {
                    if let Err(err) = syscall!(libc::close(fd)) {
                        log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
                    }
                });
                mem::forget(item.io);
            } else {
                item.flags.insert(Flags::DROPPED_SEC);
            }
            self.streams.set(Some(streams));
        } else {
            self.add_delayed_drop(IdType::Weak(id));
        }
    }

    fn add_delayed_drop(&self, id: IdType) {
        self.delayed_drop.set(true);
        if let Some(mut feed) = self.delayed_feed.take() {
            feed.push(id);
            self.delayed_feed.set(Some(feed));
        }
    }

    fn check_delayed_feed(&self) {
        if self.delayed_drop.get() {
            self.delayed_drop.set(false);
            if let Some(mut feed) = self.delayed_feed.take() {
                for id in feed.drain(..) {
                    match id {
                        IdType::Stream(id) => self.drop_stream(id),
                        IdType::Weak(id) => self.drop_weak_stream(id),
                    }
                }
                self.delayed_feed.set(Some(feed));
            }
        }
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(self) -> io::Result<()> {
        self.inner
            .with(|streams| {
                let item = &mut streams[self.id as usize];
                let fd = item.fd();
                ntex_rt::spawn_blocking(move || {
                    syscall!(libc::shutdown(fd, libc::SHUT_RDWR)).map(|_| ())
                })
            })
            .await
            .map_err(io::Error::other)
            .and_then(|res| res.map_err(io::Error::other))
    }

    /// Modify poll interest for the stream
    pub(crate) fn interest(&self, rd: bool, wr: bool) {
        self.inner.with(|streams| {
            let io = &mut streams[self.id as usize];
            let mut event = Event::new(0, false, false).with_interrupt();
            log::trace!(
                "{}: {:?}-Mod rd({rd:?}) wr({wr:?}) {:?}",
                io.tag(),
                io.fd(),
                io.flags
            );

            let mut want_update_read = true;
            if rd {
                if io.flags.contains(Flags::RD) {
                    event.readable = true;
                    want_update_read = false;
                } else if io.read().ready() {
                    event.readable = true;
                    io.flags.insert(Flags::RD);
                } else {
                    want_update_read = false;
                }
            } else if io.flags.contains(Flags::RD) {
                io.flags.remove(Flags::RD);
            } else {
                want_update_read = false;
            }

            let mut want_update_write = true;
            if wr {
                if io.flags.contains(Flags::WR) {
                    event.writable = true;
                    want_update_write = false;
                } else if io.write().ready() {
                    event.writable = true;
                    io.flags.insert(Flags::WR);
                } else {
                    want_update_write = false;
                }
            } else if io.flags.contains(Flags::WR) {
                io.flags.remove(Flags::WR);
            } else {
                want_update_write = false;
            }

            if want_update_read || want_update_write {
                log::trace!(
                    "{}: {:?}-Upd rd({:?}) wr({:?})",
                    io.tag(),
                    io.fd(),
                    event.readable,
                    event.writable
                );
                self.inner.api.modify(io.fd(), self.id, event);
            }
        })
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        self.inner.drop_stream(self.id);
    }
}

impl WeakStreamCtl {
    pub(crate) fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|streams| f(&streams[self.id as usize].io))
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        self.inner.drop_weak_stream(self.id);
    }
}

impl StreamItem {
    fn fd(&self) -> os::fd::RawFd {
        self.io.as_raw_fd()
    }

    fn tag(&self) -> &'static str {
        self.ctx.tag()
    }

    fn write(&mut self) -> IoTaskStatus {
        if let Some(buf) = self.ctx.get_write_buf() {
            let fd = self.fd();
            log::trace!("{}: {fd:?}-Wrt buf({:?})", self.ctx.tag(), buf.len());
            let res = syscall!(break libc::write(fd, buf[..].as_ptr() as _, buf.len()));
            return self.ctx.release_write_buf(buf, res);
        }
        IoTaskStatus::Pause
    }

    fn read(&mut self) -> IoTaskStatus {
        let mut buf = self.ctx.get_read_buf();

        let fd = self.fd();
        let mut total = 0;
        loop {
            self.ctx.resize_read_buf(&mut buf);

            let chunk = buf.chunk_mut();
            let chunk_len = chunk.len();
            let chunk_ptr = chunk.as_mut_ptr();

            let res = match syscall!(break libc::read(fd, chunk_ptr as _, chunk_len)) {
                Poll::Ready(Ok(size)) => {
                    total += size;
                    if size > 0 {
                        unsafe { buf.advance_mut(size) };
                        continue;
                    }
                    Poll::Ready(Err(None))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(Some(err))),
                Poll::Pending => Poll::Pending,
            };
            log::trace!("{}: {fd:?}-Rdt sz({total:?}) = {res:?}", self.tag());
            return self.ctx.release_read_buf(total, buf, res);
        }
    }
}
