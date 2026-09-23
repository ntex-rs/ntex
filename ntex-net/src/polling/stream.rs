#![allow(clippy::cast_possible_wrap)]
use std::{cell::Cell, cmp, io, mem, os, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::{BufMut, BytePage};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::{Arbiter, syscall};
use slab::Slab;
use socket2::Socket;

use super::{Event, Handler, Reactor, ReactorApi};
use crate::helpers::Queue;

const MAX_WRITE_SIZE: usize = 64 * 1024;
const MAX_WRITE_ITEMS: usize = 16;

pub(super) struct StreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

pub(super) struct WeakStreamCtl {
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
    Write(u32),
}

#[derive(Debug)]
pub(super) struct StreamItem {
    io: Socket,
    flags: Flags,
    ctx: IoContext,
}

pub(crate) struct StreamOps(Rc<StreamOpsInner>);

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

struct StreamOpsInner {
    api: ReactorApi,
    delayed_feed: Queue<IdType>,
    streams: Cell<Option<Box<Slab<StreamItem>>>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Reactor) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    delayed_feed: Queue::new(),
                    streams: Cell::new(Some(Box::new(Slab::new()))),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    /// Register new stream
    pub(crate) fn register(&self, io: Socket, ctx: IoContext) -> (StreamCtl, WeakStreamCtl) {
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

        // Hangup is never subscribed: epoll and `poll` report `HUP` and `ERR`
        // whatever the interest, and kqueue ignores the bit.
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

bitflags::bitflags! {
    /// A notification reduced to the parts the stream handler acts on.
    ///
    /// Kept separate from `Event` so the policy can be exercised on backends
    /// whose `Event` cannot represent `HUP` (kqueue reports it as absent).
    ///
    /// A peer half-close needs no bit of its own: the kernel reports it as
    /// readable, and the read that follows returns eof once buffered input is
    /// drained. `EPOLLRDHUP` would only announce it early, ahead of that
    /// input, which is of no use while read back-pressure holds the input
    /// back.
    #[derive(Copy, Clone, Debug)]
    struct Notify: u8 {
        const READABLE = 0b0000_0001;
        const WRITABLE = 0b0000_0010;
        /// Terminal condition, `EPOLLHUP` or `EPOLLERR`.
        const HUP      = 0b0000_0100;
    }
}

impl StreamOpsHandler {
    /// Apply a notification to a stream.
    fn handle_event(&mut self, id: usize, ev: Notify) {
        self.inner.with(|streams| {
            if !streams.contains(id) {
                return;
            }
            let io = &mut streams[id];
            let mut renew_rd = false;
            let mut renew_wr = false;
            #[cfg(feature = "trace")]
            log::trace!("{}: {:?}-Evt {ev:?} {:?}", io.tag(), io.fd(), io.flags);

            if ev.contains(Notify::READABLE) {
                if io.read() == IoTaskStatus::Io {
                    renew_rd = true;
                    io.flags.insert(Flags::RD);
                } else {
                    io.flags.remove(Flags::RD);
                }
            } else if io.flags.contains(Flags::RD) {
                renew_rd = true;
            }

            if ev.contains(Notify::WRITABLE) {
                if io.write() == IoTaskStatus::Io {
                    renew_wr = true;
                    io.flags.insert(Flags::WR);
                } else {
                    io.flags.remove(Flags::WR);
                }
            } else if io.flags.contains(Flags::WR) {
                renew_wr = true;
            }

            if ev.contains(Notify::HUP) {
                io.ctx.stop(None);
            } else {
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: {:?}-Renew rd({renew_rd:?}) wr({renew_wr:?})",
                    io.tag(),
                    io.fd()
                );
                self.inner
                    .api
                    .modify(io.fd(), id as u32, Event::new(0, renew_rd, renew_wr));
            }
        });
    }
}

impl Handler for StreamOpsHandler {
    fn event(&mut self, id: usize, ev: Event) {
        let mut notify = Notify::empty();
        notify.set(Notify::READABLE, ev.readable);
        notify.set(Notify::WRITABLE, ev.writable);
        // `EPOLLERR` is terminal and, like `EPOLLHUP`, is reported whether or
        // not it was requested. Re-arming on it makes no progress.
        notify.set(Notify::HUP, ev.is_interrupt() || ev.is_err() == Some(true));
        self.handle_event(id, notify);
    }

    fn error(&mut self, id: usize, err: io::Error) {
        self.inner.with(|streams| {
            if let Some(io) = streams.get_mut(id) {
                log::trace!("{}: {:?}-Failed err({err:?})", io.tag(), io.fd());
                io.ctx.stop(Some(err));
            }
        });
    }

    fn tick(&mut self) {
        self.inner.check_delayed_feed();
    }

    fn cleanup(&mut self) {
        if let Some(v) = self.inner.streams.take() {
            for (_, val) in v.into_iter() {
                if !val.flags.contains(Flags::DROPPED_PRI) {
                    log::trace!(
                        "{}: Unclosed sockets {:?}",
                        val.ctx.tag(),
                        val.io.peer_addr()
                    );
                }
                // A close job removes its entry; sockets still in the slab are ours.
                drop(val);
            }
        }
        self.inner.delayed_feed.clear();
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

    /// Initiate write operation for the stream
    fn write_stream(&self, id: u32) {
        if let Some(mut streams) = self.streams.take() {
            if let Some(item) = streams.get_mut(id as usize) {
                self.write_item(id, item);
            }
            self.streams.set(Some(streams));
        } else {
            // Write is initiated while `StreamOps` is handling an event
            // (e.g. a filter generated data during read processing).
            // Delay the write until the streams slab is released,
            // dropping it would stall the connection.
            self.delayed_feed.push(IdType::Write(id));
        }
    }

    /// Writes buffered output and arms write interest if any is left.
    ///
    /// This runs outside the write task, on the stack of whoever filled the
    /// write buffer. Arming here rather than leaving it to the write task
    /// saves a write syscall: `interest()` finds `Flags::WR` already set and
    /// returns without retrying the write.
    ///
    /// Interest is only ever added. Output that drained completely leaves the
    /// arming in place and costs at most one spurious writable event, which
    /// `handle_event` clears; disarming here would cost a syscall on the
    /// common path instead.
    fn write_item(&self, id: u32, item: &mut StreamItem) {
        if item.write() == IoTaskStatus::Io && !item.flags.contains(Flags::WR) {
            item.flags.insert(Flags::WR);
            let event = Event::new(0, item.flags.contains(Flags::RD), true);
            self.api.modify(item.fd(), id, event);
        }
    }

    fn drop_stream(&self, id: u32, streams: &mut Slab<StreamItem>) {
        // Dropping while `StreamOps` handling event
        let idx = id as usize;
        let item = &mut streams[idx];
        let fd = item.fd();
        log::trace!("{}: {fd:?}-Close flags: {:?}", item.tag(), item.flags);

        item.ctx.stop(None);
        self.api.detach(fd, id);

        if item.flags.contains(Flags::DROPPED_SEC) {
            let item = streams.remove(idx);
            ntex_rt::spawn_blocking(move || {
                if let Err(err) = syscall!(libc::close(fd)) {
                    log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
                }
            })
            .detach();
            mem::forget(item.io);
        } else {
            item.flags.insert(Flags::DROPPED_PRI);
        }
    }

    fn drop_weak_stream(id: u32, streams: &mut Slab<StreamItem>) {
        // Dropping while `StreamOps` handling event
        let idx = id as usize;
        let item = &mut streams[idx];

        if item.flags.contains(Flags::DROPPED_PRI) {
            let item = streams.remove(idx);
            let fd = item.fd();
            ntex_rt::spawn_blocking(move || {
                if let Err(err) = syscall!(libc::close(fd)) {
                    log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
                }
            })
            .detach();
            mem::forget(item.io);
        } else {
            item.flags.insert(Flags::DROPPED_SEC);
        }
    }

    fn check_delayed_feed(&self) {
        if !self.delayed_feed.is_empty()
            && let Some(mut streams) = self.streams.take()
        {
            while let Some(id) = self.delayed_feed.pop() {
                match id {
                    IdType::Stream(id) => self.drop_stream(id, &mut streams),
                    IdType::Weak(id) => StreamOpsInner::drop_weak_stream(id, &mut streams),
                    IdType::Write(id) => {
                        if let Some(item) = streams.get_mut(id as usize) {
                            self.write_item(id, item);
                        }
                    }
                }
            }
            self.streams.set(Some(streams));
        }
    }

    /// Modify poll interest for the stream
    fn interest(&self, id: u32, rd: bool, wr: bool) {
        self.with(|streams| {
            let io = &mut streams[id as usize];
            let mut event_rd = false;
            let mut event_wr = false;
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: {:?}-Mod rd({rd:?}) wr({wr:?}) {:?}",
                io.tag(),
                io.fd(),
                io.flags
            );

            let mut want_update_read = true;
            if rd {
                if io.flags.contains(Flags::RD) {
                    event_rd = true;
                    want_update_read = false;
                } else if io.read() == IoTaskStatus::Io {
                    event_rd = true;
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
                    event_wr = true;
                    want_update_write = false;
                } else if io.write() == IoTaskStatus::Io {
                    event_wr = true;
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
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: {:?}-Upd rd({event_rd:?}) wr({event_wr:?})",
                    io.tag(),
                    io.fd()
                );
                self.api
                    .modify(io.fd(), id, Event::new(0, event_rd, event_wr));
            }
        });
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(self) -> io::Result<()> {
        self.inner
            .with(|streams| {
                let item = &mut streams[self.id as usize];
                let fd = item.fd();
                // The socket is still registered with the poller at this
                // point, so it is drained here rather than on the blocking
                // pool: reading it from another thread would race the reactor.
                // The drain is non-blocking and bounded, so it is cheap enough
                // to run inline.
                crate::helpers::drain_raw_socket(fd);
                ntex_rt::spawn(ntex_rt::spawn_blocking(move || {
                    syscall!(libc::shutdown(fd, libc::SHUT_RDWR)).map(|_| ())
                }))
            })
            .await
            .map_err(io::Error::other)
            .and_then(|res| res.map_err(io::Error::other))
            .and_then(|res| res)
    }

    /// Arranges for the socket to be aborted instead of closed gracefully.
    ///
    /// The descriptor itself is released when this handle is dropped.
    pub(crate) fn abort(&self) {
        self.inner.with(|streams| {
            crate::helpers::abort_raw_socket(streams[self.id as usize].fd());
        });
    }

    /// Modify poll interest for the stream
    pub(crate) fn interest(&self, rd: bool, wr: bool) {
        self.inner.interest(self.id, rd, wr);
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        if let Some(mut streams) = self.inner.streams.take() {
            self.inner.drop_stream(self.id, &mut streams);
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayed_feed.push(IdType::Stream(self.id));
        }
    }
}

impl WeakStreamCtl {
    pub(super) fn with_socket<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Socket) -> R,
    {
        self.inner.with(|streams| f(&streams[self.id as usize].io))
    }

    /// Initiate write operation for the stream
    pub(super) fn write(&self) {
        self.inner.write_stream(self.id);
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        if let Some(mut streams) = self.inner.streams.take() {
            StreamOpsInner::drop_weak_stream(self.id, &mut streams);
            self.inner.streams.set(Some(streams));
        } else {
            self.inner.delayed_feed.push(IdType::Weak(self.id));
        }
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
        let res = self.ctx.with_write_dst(|wrt| {
            let mut pages: [Option<BytePage>; MAX_WRITE_ITEMS] = [
                None, None, None, None, None, None, None, None, None, None, None, None, None, None,
                None, None,
            ];
            let mut bufs: [mem::MaybeUninit<io::IoSlice<'_>>; MAX_WRITE_ITEMS] =
                [mem::MaybeUninit::uninit(); MAX_WRITE_ITEMS];

            let mut num = 0;
            let mut size = 0;
            while let Some(page) = wrt.take() {
                size += page.len();

                // SAFETY: Page is stored in `pages` for lifetime of `bufs`
                bufs[num] = mem::MaybeUninit::new(io::IoSlice::new(unsafe {
                    mem::transmute::<&[u8], &[u8]>(page.as_ref())
                }));
                pages[num] = Some(page);

                num += 1;
                if num == MAX_WRITE_ITEMS || size >= MAX_WRITE_SIZE {
                    break;
                }
            }

            if num > 0 {
                let fd = self.fd();
                // An error must not return early: the pages taken above still
                // have to go back to the write buffer.
                let res = if num == 1 {
                    let io = unsafe { bufs[0].assume_init_ref().as_ptr() };
                    syscall!(break libc::write(fd, io.cast(), size))
                } else {
                    syscall!(break libc::writev(fd, bufs.as_ptr().cast(), num as i32))
                };
                #[cfg(feature = "trace")]
                log::trace!("{}: {fd:?}-Wrt buf({num}:{size}) ({res:?})", self.ctx.tag());

                // remove written bytes
                if let Poll::Ready(Ok(mut written)) = res {
                    for page in pages[..num].iter_mut().flatten() {
                        let len = cmp::min(page.len(), written);
                        page.advance_to(len);
                        written -= len;
                        if written == 0 {
                            break;
                        }
                    }
                }
                // return unwritten data back to buffer
                for p in pages[..num].iter_mut().rev() {
                    if let Some(page) = p.take() {
                        wrt.prepend(page);
                    }
                }

                match res? {
                    Poll::Ready(0) => Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write frame to transport",
                    )),
                    Poll::Ready(n) => Ok(n),
                    Poll::Pending => Ok(0),
                }
            } else {
                Ok(0)
            }
        });
        self.ctx.update_write_status(res)
    }

    fn read(&mut self) -> IoTaskStatus {
        let fd = self.fd();
        #[cfg(feature = "trace")]
        let tag = self.tag();

        self.ctx.with_read_buf(|buf| {
            let chunk = buf.chunk_mut();
            let chunk_len = chunk.len();
            let chunk_ptr = chunk.as_mut_ptr();

            let result = syscall!(break libc::read(fd, chunk_ptr.cast(), chunk_len));
            #[cfg(feature = "trace")]
            log::trace!("{tag}: {fd:?}-Rdt sz() = {result:?}");

            if let Poll::Ready(Ok(n)) = result
                && n != 0
            {
                unsafe { buf.advance_mut(n) };
            }
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use ntex_io::{Handle, Io, IoStream};
    use ntex_service::cfg::SharedCfg;

    use super::*;

    struct TestStream {
        socket: Socket,
        ops: StreamOps,
        ctl: Rc<Cell<Option<StreamCtl>>>,
    }

    struct TestHandle {
        _ctl: WeakStreamCtl,
    }

    impl Handle for TestHandle {}

    impl IoStream for TestStream {
        fn start(self, ctx: IoContext) -> Box<dyn Handle> {
            let (ctl, weak) = self.ops.register(self.socket, ctx);
            self.ctl.set(Some(ctl));
            Box::new(TestHandle { _ctl: weak })
        }
    }

    #[ntex::test]
    async fn cleanup_closes_socket_with_deferred_secondary_drop() {
        let reactor = Reactor::new().unwrap();
        let ops = StreamOps::get(&reactor);
        let (socket, _peer) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let ctl = Rc::new(Cell::new(None));
        let io = Io::new(
            TestStream {
                socket: Socket::from(socket),
                ops: ops.clone(),
                ctl: ctl.clone(),
            },
            SharedCfg::default(),
        );
        let primary = ctl.take().unwrap();
        let id = primary.id as usize;
        drop(primary);
        assert!(
            ops.0
                .with(|streams| streams[id].flags.contains(Flags::DROPPED_PRI))
        );
        assert!(!ops.0.delayed_feed.is_empty());
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);

        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };
        handler.cleanup();

        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        let err = io::Error::last_os_error();
        if result != -1 {
            // Release the leaked descriptor if this regression returns.
            assert_eq!(unsafe { libc::close(fd) }, 0);
        }
        assert_eq!(result, -1, "cleanup leaked the socket");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        assert!(ops.0.delayed_feed.is_empty());
        handler.cleanup();
        drop(io);
    }

    struct Fixture {
        io: Io,
        ops: StreamOps,
        handler: StreamOpsHandler,
        id: usize,
        peer: UnixStream,
        _ctl: StreamCtl,
        _reactor: Reactor,
    }

    impl Fixture {
        fn new() -> Self {
            let reactor = Reactor::new().unwrap();
            let ops = StreamOps::get(&reactor);
            let (socket, peer) = UnixStream::pair().unwrap();
            socket.set_nonblocking(true).unwrap();
            let ctl = Rc::new(Cell::new(None));
            let io = Io::new(
                TestStream {
                    socket: Socket::from(socket),
                    ops: ops.clone(),
                    ctl: ctl.clone(),
                },
                SharedCfg::default(),
            );
            let ctl = ctl.take().unwrap();
            Fixture {
                id: ctl.id as usize,
                handler: StreamOpsHandler {
                    inner: ops.0.clone(),
                },
                io,
                ops,
                peer,
                _ctl: ctl,
                _reactor: reactor,
            }
        }

        /// Arm read interest, mirroring what `interest(id, true, _)` leaves
        /// behind once the io layer has asked for reads.
        fn arm_read(&self) {
            self.ops
                .0
                .with(|streams| streams[self.id].flags.insert(Flags::RD));
        }

        fn fire(&mut self, notify: Notify) {
            let id = self.id;
            self.handler.handle_event(id, notify);
        }

        fn flags(&self) -> Flags {
            self.ops.0.with(|streams| streams[self.id].flags)
        }

        fn teardown(mut self) {
            self.handler.cleanup();
        }
    }

    /// Half-close and terminal-condition handling.
    ///
    /// These drive `handle_event` directly, so the policy is covered on every
    /// platform even though only epoll ever reports `ERR`.
    mod hup {
        use std::net::Shutdown;

        use super::*;

        /// A peer half-close is reported as readable. It must surface as eof,
        /// not as a hard stop: buffered output still has to drain.
        #[ntex::test]
        async fn half_close_with_read_armed_delivers_eof() {
            let mut fixture = Fixture::new();
            fixture.arm_read();
            fixture.peer.shutdown(Shutdown::Write).unwrap();

            fixture.fire(Notify::READABLE);

            let eof = fixture.io.is_read_eof();
            let closed = !fixture.io.is_active();
            fixture.teardown();

            assert!(eof, "half-close did not reach the io layer");
            assert!(!closed, "half-close terminated the connection");
        }

        /// `EPOLLERR` is terminal and is reported whether or not it was
        /// requested, so it must stop the stream instead of re-arming.
        #[ntex::test]
        async fn err_stops_the_stream() {
            let mut fixture = Fixture::new();
            fixture.arm_read();

            fixture.fire(Notify::HUP);

            let closed = !fixture.io.is_active();
            fixture.teardown();

            assert!(closed, "EPOLLERR did not stop the stream");
        }
    }
    /// Out-of-band writes, performed on the stack of whoever filled the write
    /// buffer rather than by the write task.
    mod write {
        use super::*;

        /// More than one `MAX_WRITE_SIZE` chunk, so output is guaranteed to be
        /// left over whatever the socket accepts.
        const LARGE: usize = MAX_WRITE_SIZE * 2;

        /// Output left over after a direct write must arm write interest here.
        /// Leaving it to the write task costs an extra write syscall, and
        /// setting `Flags::WR` without telling the reactor would stall the
        /// connection: `interest()` would believe it was already armed.
        #[ntex::test]
        async fn direct_write_arms_write_interest() {
            let fixture = Fixture::new();
            fixture.io.encode_slice(&vec![b'x'; LARGE]).unwrap();
            assert!(!fixture.flags().contains(Flags::WR));

            fixture.ops.0.write_stream(fixture.id as u32);

            let armed = fixture.flags().contains(Flags::WR);
            fixture.teardown();

            assert!(armed, "leftover output did not arm write interest");
        }

        /// Output that drained completely needs no interest.
        #[ntex::test]
        async fn drained_write_does_not_arm_write_interest() {
            let fixture = Fixture::new();
            fixture.io.encode_slice(b"small").unwrap();

            fixture.ops.0.write_stream(fixture.id as u32);

            let armed = fixture.flags().contains(Flags::WR);
            fixture.teardown();

            assert!(!armed, "drained output armed write interest");
        }

        /// Arming is additive, so a second write must not disturb it.
        #[ntex::test]
        async fn repeated_direct_writes_keep_interest_armed() {
            let fixture = Fixture::new();
            fixture.io.encode_slice(&vec![b'x'; LARGE]).unwrap();
            fixture.ops.0.write_stream(fixture.id as u32);
            fixture.ops.0.write_stream(fixture.id as u32);

            let armed = fixture.flags().contains(Flags::WR);
            fixture.teardown();

            assert!(armed, "write interest was lost on a repeat write");
        }

        /// A failed write must hand the pages it took back to the write
        /// buffer. Dropping them loses the output and leaves it counted as in
        /// flight, which nothing will ever report as written.
        #[ntex::test]
        async fn failed_write_returns_pages_to_the_buffer() {
            let mut fixture = Fixture::new();
            fixture.io.encode_slice(b"hello").unwrap();
            // Closing the peer makes the next write fail with `EPIPE`.
            drop(mem::replace(
                &mut fixture.peer,
                UnixStream::pair().unwrap().0,
            ));

            fixture.ops.0.write_stream(fixture.id as u32);

            let buffered = fixture.io.with_write_dst(|b| b.len());
            let closed = !fixture.io.is_active();
            fixture.teardown();

            assert!(closed, "write error did not stop the stream");
            assert_eq!(buffered, 5, "failed write dropped its pages");
        }
    }
}
