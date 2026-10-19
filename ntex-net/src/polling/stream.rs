#![allow(clippy::cast_possible_wrap)]
use std::{cell::Cell, cmp, io, mem, os, os::fd::AsRawFd, rc::Rc, task::Poll};

use ntex_bytes::{BufMut, BytePages};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::{Arbiter, syscall};
use slab::Slab;
use socket2::Socket;

use super::{Event, Handler, PollMode, Reactor, ReactorApi};
use crate::helpers::Queue;

const MAX_WRITE_SIZE: usize = 256 * 1024;
const MAX_WRITE_ITEMS: usize = 32;
/// Output written per write attempt at most, see `StreamItem::write()`.
const WRITE_BUDGET: usize = 256 * 1024;
const READ_BUDGET: usize = 256 * 1024;

pub(super) struct StreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

pub(super) struct WeakStreamCtl {
    id: u32,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    struct Flags: u8 {
        const RD          = 0b0000_0001;
        const WR          = 0b0000_0010;
        const DETACHED    = 0b0000_0100;
        /// Reads are paused while read interest stays armed, it is disarmed
        /// by the next readable event. A resume before new data arrives then
        /// needs neither an interest update nor a speculative read.
        const RD_PAUSED   = 0b0000_1000;
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
    mode: PollMode,
    delayed_feed: Queue<IdType>,
    streams: Cell<Option<Box<Slab<StreamItem>>>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(driver: &Reactor) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            driver.register(|api| {
                // Level-triggered interest stays armed after an event, so it
                // only has to be modified when it changes.
                let mode = if api.supports_level() {
                    PollMode::Level
                } else {
                    PollMode::Oneshot
                };
                let ops = Rc::new(StreamOpsInner {
                    api,
                    mode,
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
                flags: Flags::RD,
            };
            StreamCtl {
                id: streams.insert(item) as u32,
                inner: self.0.clone(),
            }
        });

        // Read interest is armed from the start, a new connection almost
        // always reads first. The io task then finds `Flags::RD` set and needs
        // neither a speculative read nor an interest update.
        //
        // Hangup is never subscribed: epoll and `poll` report `HUP` and `ERR`
        // whatever the interest, and kqueue ignores the bit.
        self.0
            .api
            .attach_with_mode(fd, stream.id, Event::new(0, true, false), self.0.mode);

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
    /// Apply a notification to a stream.
    ///
    /// A peer half-close is reported as readable, and the read that follows
    /// returns eof once buffered input is drained. `EPOLLRDHUP` would only
    /// announce it early, ahead of that input, which is of no use while read
    /// back-pressure holds the input back.
    fn event(&mut self, id: usize, ev: Event) {
        self.inner.with(|streams| {
            if !streams.contains(id) {
                return;
            }
            let io = &mut streams[id];
            // kqueue reports read and write separately, an earlier event of
            // the same poll may have detached the stream.
            if io.flags.contains(Flags::DETACHED) {
                return;
            }
            let prev = io.flags & (Flags::RD | Flags::WR);
            let mut renew_rd = false;
            let mut renew_wr = false;
            #[cfg(feature = "trace")]
            log::trace!("{}: {:?}-Evt {ev:?} {:?}", io.tag(), io.fd(), io.flags);

            if ev.readable {
                if io.flags.contains(Flags::RD_PAUSED) {
                    // Data arrived while reads are paused
                    io.flags.remove(Flags::RD | Flags::RD_PAUSED);
                } else {
                    match io.read() {
                        (IoTaskStatus::Io, _) => {
                            renew_rd = true;
                            io.flags.insert(Flags::RD);
                        }
                        // The socket is drained, no readable event is expected
                        // before the reads resume. Leftover data would report
                        // one right away, so interest is disarmed eagerly then.
                        (IoTaskStatus::Pause, true) => {
                            renew_rd = true;
                            io.flags.insert(Flags::RD | Flags::RD_PAUSED);
                        }
                        _ => io.flags.remove(Flags::RD),
                    }
                }
            } else if io.flags.contains(Flags::RD) {
                renew_rd = true;
            }

            if ev.writable {
                if io.write() == IoTaskStatus::Io {
                    renew_wr = true;
                    io.flags.insert(Flags::WR);
                } else {
                    io.flags.remove(Flags::WR);
                }
            } else if io.flags.contains(Flags::WR) {
                renew_wr = true;
            }

            // `EPOLLERR` and `EPOLLHUP` are terminal and are reported whether
            // or not they were requested. Re-arming on them makes no progress,
            // and level-triggered interest would keep reporting them, so the
            // stream is detached. kqueue reports neither, failures surface
            // through reads and writes instead.
            if ev.is_err() == Some(true) {
                // The error is not surfaced by a read or write when their
                // interest is not armed, so it is taken from the socket. It
                // may already have been consumed by an earlier operation.
                let err = io.io.take_error().ok().flatten().unwrap_or_else(|| {
                    io::Error::new(io::ErrorKind::ConnectionReset, "transport error")
                });
                io.ctx.stop(Some(err));
                self.inner.detach(id as u32, io);
            } else if ev.is_interrupt() {
                io.ctx.stop(None);
                self.inner.detach(id as u32, io);
            } else if self.inner.mode != PollMode::Level
                || prev != io.flags & (Flags::RD | Flags::WR)
            {
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: {:?}-Renew rd({renew_rd:?}) wr({renew_wr:?})",
                    io.tag(),
                    io.fd()
                );
                self.inner.modify(id as u32, io, renew_rd, renew_wr);
            }
        });
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
    /// `event` clears; disarming here would cost a syscall on the
    /// common path instead.
    fn write_item(&self, id: u32, item: &mut StreamItem) {
        if item.write() == IoTaskStatus::Io && !item.flags.contains(Flags::WR) {
            item.flags.insert(Flags::WR);
            self.modify(id, item, item.flags.contains(Flags::RD), true);
        }
    }

    /// Sets poll interest, `Flags::RD` and `Flags::WR` must match it.
    fn modify(&self, id: u32, item: &StreamItem, rd: bool, wr: bool) {
        if !item.flags.contains(Flags::DETACHED) {
            self.api
                .modify_with_mode_deferred(item.fd(), id, Event::new(0, rd, wr), self.mode);
        }
    }

    fn detach(&self, id: u32, item: &mut StreamItem) {
        if !item.flags.contains(Flags::DETACHED) {
            item.flags.insert(Flags::DETACHED);
            self.api.detach(item.fd(), id);
        }
    }

    fn drop_stream(&self, id: u32, streams: &mut Slab<StreamItem>) {
        // Dropping while `StreamOps` handling event
        let idx = id as usize;
        let item = &mut streams[idx];
        let fd = item.fd();
        log::trace!("{}: {fd:?}-Close flags: {:?}", item.tag(), item.flags);

        item.ctx.stop(None);
        self.detach(id, item);

        if item.flags.contains(Flags::DROPPED_SEC) {
            let item = streams.remove(idx);
            close_fd(fd);
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
            close_fd(item.fd());
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

            let mut want_update_read = false;
            if rd {
                if io.flags.contains(Flags::RD) {
                    event_rd = true;
                    io.flags.remove(Flags::RD_PAUSED);
                } else if io.read().0 == IoTaskStatus::Io {
                    event_rd = true;
                    want_update_read = true;
                    io.flags.insert(Flags::RD);
                }
            } else if io.flags.contains(Flags::RD) {
                // Interest stays armed, the next readable event disarms it
                event_rd = true;
                io.flags.insert(Flags::RD_PAUSED);
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
                self.modify(id, io, event_rd, event_wr);
            }
        });
    }
}

impl StreamCtl {
    /// Drains the socket and shuts it down.
    ///
    /// Both steps run inline: they do not block, and the descriptor is only
    /// valid while this handle is alive. A job deferred to the blocking pool
    /// could outlive the handle, if the runtime stops first, and then act on
    /// an unrelated socket that reused the descriptor.
    pub(crate) fn shutdown(self) -> io::Result<()> {
        self.inner.with(|streams| {
            let fd = streams[self.id as usize].fd();
            crate::helpers::drain_raw_socket(fd);
            crate::helpers::shutdown_result(
                syscall!(libc::shutdown(fd, libc::SHUT_RDWR)).map(|_| ()),
            )
        })
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

    /// Writes buffered output until the socket is full or the write budget is
    /// spent.
    ///
    /// A single `writev` is limited to `MAX_WRITE_SIZE`, so writes continue
    /// while the socket accepts everything, a large buffer does not wait for
    /// a poll cycle per chunk. A short write means the socket is full and the
    /// next one would only return `EAGAIN`. `WRITE_BUDGET` keeps one
    /// connection from delaying the others.
    fn write(&mut self) -> IoTaskStatus {
        let fd = self.fd();
        let tag = self.ctx.tag();
        let res = self.ctx.with_write_dst(|wrt| {
            let mut total = 0;
            loop {
                let (written, size) = write_pages(fd, tag, wrt)?;
                total += written;
                if written == 0 || written < size || total >= WRITE_BUDGET {
                    return Ok(total);
                }
            }
        });
        self.ctx.update_write_status(res)
    }

    /// A read that fills the buffer chunk likely left more input in the
    /// socket, so reads continue until a short read or `READ_BUDGET` instead
    /// of waiting for the next poll cycle.
    /// Reads until the socket would block or the budget is used up.
    ///
    /// Also reports whether the last read drained the socket.
    fn read(&mut self) -> (IoTaskStatus, bool) {
        let fd = self.fd();
        #[cfg(feature = "trace")]
        let tag = self.tag();

        let mut total = 0;
        loop {
            let mut full = false;
            let status = self.ctx.with_read_buf(|buf| {
                let chunk = buf.chunk_mut();
                let chunk_len = chunk.len();
                let chunk_ptr = chunk.as_mut_ptr();

                let result = syscall!(break libc::read(fd, chunk_ptr.cast(), chunk_len));
                #[cfg(feature = "trace")]
                log::trace!("{tag}: {fd:?}-Rdt sz() = {result:?}");

                if let Poll::Ready(Ok(n)) = result
                    && n != 0
                {
                    full = n == chunk_len;
                    total += n;
                    unsafe { buf.advance_mut(n) };
                }
                result
            });
            if status != IoTaskStatus::Io || !full || total >= READ_BUDGET {
                return (status, !full);
            }
        }
    }
}

/// Closes the descriptor of a dropped stream.
///
/// Closing a non-blocking socket does not wait, except on platforms where a
/// non-zero `SO_LINGER` timeout makes `close()` wait for unsent output even
/// then. Only the application can set it, before handing the stream over, and
/// such a close runs in the blocking pool.
fn close_fd(fd: os::fd::RawFd) {
    fn close(fd: os::fd::RawFd) {
        if let Err(err) = syscall!(libc::close(fd)) {
            log::error!("Cannot close file descriptor ({fd:?}), {err:?}");
        }
    }

    if lingers(fd) {
        ntex_rt::spawn_blocking(move || close(fd)).detach();
    } else {
        close(fd);
    }
}

/// Whether closing the socket waits for a linger timeout.
///
/// The BSDs and macOS do not wait on a non-blocking socket.
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
)))]
fn lingers(fd: os::fd::RawFd) -> bool {
    // SAFETY: the fd is owned by the dropped stream and is not closed yet
    let fd = unsafe { os::fd::BorrowedFd::borrow_raw(fd) };
    matches!(socket2::SockRef::from(&fd).linger(), Ok(Some(t)) if !t.is_zero())
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly",
))]
fn lingers(_: os::fd::RawFd) -> bool {
    false
}

/// Writes up to `MAX_WRITE_ITEMS` pages or `MAX_WRITE_SIZE` bytes of `wrt`.
///
/// Returns the written bytes and the size of the attempted write, both are
/// zero if `wrt` is empty. A write that would block writes zero bytes.
#[allow(clippy::used_underscore_binding)]
fn write_pages(fd: os::fd::RawFd, _tag: &str, wrt: &mut BytePages) -> io::Result<(usize, usize)> {
    let pages = wrt.num_pages();
    let (res, size) = if pages <= 1 {
        // a single page is written without building the slice array
        let Some(chunk) = wrt.iter().next() else {
            return Ok((0, 0));
        };
        let size = chunk.len();
        let res = syscall!(break libc::write(fd, chunk.as_ptr().cast(), size));
        (res, size)
    } else {
        let mut bufs = [io::IoSlice::new(&[]); MAX_WRITE_ITEMS];
        let mut num = 0;
        let mut size = 0;
        for chunk in wrt.iter() {
            bufs[num] = io::IoSlice::new(chunk);
            num += 1;
            size += chunk.len();
            if num == MAX_WRITE_ITEMS || size >= MAX_WRITE_SIZE {
                break;
            }
        }
        let res = syscall!(break libc::writev(fd, bufs.as_ptr().cast(), num as i32));
        (res, size)
    };
    #[cfg(feature = "trace")]
    log::trace!("{_tag}: {fd:?}-Wrt buf({pages}:{size}) ({res:?})");

    // remove written bytes
    if let Poll::Ready(Ok(written)) = res {
        wrt.advance_to(cmp::min(written, size));
    }

    match res? {
        Poll::Ready(0) => Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write frame to transport",
        )),
        Poll::Ready(n) => Ok((n, size)),
        Poll::Pending => Ok((0, size)),
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
        // The close is observed through the peer, the descriptor number can
        // be reused by tests running in parallel.
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let peer_closed = |peer: &mut UnixStream| match io::Read::read(peer, &mut [0u8; 1]) {
            Ok(0) => true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            res => panic!("unexpected: {res:?}"),
        };
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
        assert!(!peer_closed(&mut peer));

        let mut handler = StreamOpsHandler {
            inner: ops.0.clone(),
        };
        handler.cleanup();

        assert!(peer_closed(&mut peer), "cleanup leaked the socket");
        assert!(ops.0.delayed_feed.is_empty());
        handler.cleanup();
        drop(io);
    }

    /// Registers `socket` and drops both stream handles.
    fn drop_registered(socket: Socket) {
        let reactor = Reactor::new().unwrap();
        let ops = StreamOps::get(&reactor);
        let ctl = Rc::new(Cell::new(None));
        let io = Io::new(
            TestStream {
                socket,
                ops: ops.clone(),
                ctl: ctl.clone(),
            },
            SharedCfg::default(),
        );
        drop(ctl.take().unwrap());
        drop(io);
        StreamOpsHandler {
            inner: ops.0.clone(),
        }
        .tick();
        assert!(ops.0.with(|streams| streams.is_empty()));
    }

    /// A dropped stream is closed inline, not in the blocking pool.
    #[ntex::test]
    async fn dropped_stream_closes_inline() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        drop_registered(Socket::from(socket));
        assert_eq!(io::Read::read(&mut peer, &mut [0u8; 1]).unwrap(), 0);
    }

    /// An aborted stream has a zero linger timeout, its close does not wait.
    #[test]
    fn zero_linger_does_not_wait() {
        let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sock = std::net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        assert!(!lingers(sock.as_raw_fd()));
        crate::helpers::abort_raw_socket(sock.as_raw_fd());
        assert!(!lingers(sock.as_raw_fd()));
    }

    /// A close that waits for an application-set linger timeout runs in the
    /// blocking pool, and still delivers the unsent output.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[allow(clippy::large_stack_arrays)]
    #[ntex::test]
    async fn lingering_close_runs_in_blocking_pool() {
        use std::time::{Duration, Instant};

        let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sock = std::net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        let (mut peer, _) = lst.accept().unwrap();
        let sock = Socket::from(sock);
        sock.set_nonblocking(true).unwrap();
        sock.set_linger(Some(Duration::from_secs(5))).unwrap();
        assert!(lingers(sock.as_raw_fd()));

        let mut sent = 0;
        loop {
            match sock.send(&[0u8; 65536]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e:?}"),
            }
        }

        let start = Instant::now();
        drop_registered(sock);
        assert!(start.elapsed() < Duration::from_secs(1), "close waited");

        peer.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut received = Vec::new();
        io::Read::read_to_end(&mut peer, &mut received).unwrap();
        assert_eq!(received.len(), sent);
    }

    #[test]
    fn runtime_tick_closes_socket_without_new_events() {
        let reactor = Reactor::new().unwrap();
        let rt = ntex_rt::Runtime::builder()
            .event_interval(1)
            .build(ntex_rt::Driver::handle(&reactor));
        let ops = StreamOps::get(&reactor);
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
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
        let (tx, rx) = oneshot::async_channel();
        let reader = std::thread::spawn(move || {
            let result = io::Read::read(&mut peer, &mut [0u8; 1]);
            tx.send(result).unwrap();
        });

        let (result, pending) = rt.block_on(
            async {
                // Consume the initial runtime notification before queuing the
                // drop, so an after-wait tick cannot mask the missing pre-wait tick.
                let mut yielded = false;
                std::future::poll_fn(|cx| {
                    if yielded {
                        Poll::Ready(())
                    } else {
                        yielded = true;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
                drop(primary);
                assert!(!ops.0.delayed_feed.is_empty());

                let result = rx.await.unwrap();
                let pending =
                    !ops.0.delayed_feed.is_empty() || ops.0.with(|streams| !streams.is_empty());
                drop(io);
                (result, pending)
            },
            &reactor,
        );
        reader.join().unwrap();

        // No shutdown() was issued: EOF proves close() ran before reactor cleanup.
        assert_eq!(result.unwrap(), 0);
        assert!(!pending, "deferred socket cleanup was not drained");
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
            Self::with_cfg(SharedCfg::default())
        }

        fn with_cfg(cfg: impl Into<SharedCfg>) -> Self {
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
                cfg.into(),
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

        fn fire(&mut self, ev: Event) {
            let id = self.id;
            self.handler.event(id, ev);
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
    /// Only epoll reports `HUP` and `ERR`, so those tests run on Linux.
    mod hup {
        use std::net::Shutdown;

        use super::*;

        /// A peer half-close is reported as readable. It must surface as eof,
        /// not as a hard stop: buffered output still has to drain.
        #[ntex::test]
        async fn half_close_with_read_armed_delivers_eof() {
            let mut fixture = Fixture::new();
            fixture.peer.shutdown(Shutdown::Write).unwrap();

            fixture.fire(Event::readable(0));

            let eof = fixture.io.is_read_eof();
            let closed = !fixture.io.is_active();
            fixture.teardown();

            assert!(eof, "half-close did not reach the io layer");
            assert!(!closed, "half-close terminated the connection");
        }

        /// A plain `EPOLLHUP` is a clean close.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[ntex::test]
        async fn hup_stops_the_stream_cleanly() {
            let mut fixture = Fixture::new();

            fixture.fire(Event::none(0).with_interrupt());

            let closed = !fixture.io.is_active();
            let err = peer_gone_error(&fixture.io).await;
            fixture.teardown();

            assert!(closed, "EPOLLHUP did not stop the stream");
            assert!(err.is_none(), "EPOLLHUP reported an error: {err:?}");
        }

        /// A terminal event detaches the stream, later events and interest
        /// changes are ignored instead of spinning or failing in the poller.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[ntex::test]
        async fn hup_detaches_the_stream() {
            let mut fixture = Fixture::new();

            fixture.fire(Event::none(0).with_interrupt());
            let detached = fixture.flags().contains(Flags::DETACHED);
            // read interest is armed at registration, clear it so a handled
            // readable event would show up
            fixture
                .ops
                .0
                .with(|streams| streams[fixture.id].flags.remove(Flags::RD));
            fixture.fire(Event::readable(0));
            let flags = fixture.flags();
            fixture.ops.0.interest(fixture.id as u32, true, true);
            fixture.teardown();

            assert!(detached, "EPOLLHUP did not detach the stream");
            assert!(!flags.contains(Flags::RD), "event after detach was handled");
        }

        /// `EPOLLERR` is terminal and is reported whether or not it was
        /// requested. A reset reported by it while read interest is not armed
        /// stops the stream with the socket error.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[ntex::test]
        async fn err_without_read_armed_reports_socket_error() {
            use ::ntex_polling::{Events, PollMode, Poller};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (socket, _) = listener.accept().unwrap();
            socket.set_nonblocking(true).unwrap();

            // a separate poller observes the kernel event for the socket
            let poller = Poller::new().unwrap();
            unsafe {
                poller
                    .add_with_mode(&socket, Event::none(0), PollMode::Oneshot)
                    .unwrap();
            }

            let reactor = Reactor::new().unwrap();
            let ops = StreamOps::get(&reactor);
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
            // read interest is armed at registration
            ops.0
                .with(|streams| streams[ctl.id as usize].flags.remove(Flags::RD));
            let mut handler = StreamOpsHandler {
                inner: ops.0.clone(),
            };

            // abortive close sends a reset
            let peer = Socket::from(peer);
            peer.set_linger(Some(std::time::Duration::ZERO)).unwrap();
            drop(peer);

            let mut events = Events::new();
            poller
                .wait(&mut events, Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let ev = events.iter().next().expect("no event for the reset");
            assert_eq!(ev.is_err(), Some(true));

            handler.event(ctl.id as usize, ev);

            let err = peer_gone_error(&io).await;
            handler.cleanup();
            drop(ctl);

            assert_eq!(err.and_then(|e| e.raw_os_error()), Some(libc::ECONNRESET));
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        async fn peer_gone_error(io: &Io) -> Option<io::Error> {
            match std::future::poll_fn(|cx| io.poll_status_update(cx)).await {
                ntex_io::IoStatusUpdate::PeerGone(err) => err,
                st => panic!("unexpected status {st:?}"),
            }
        }
    }

    /// Pollers that support it use level-triggered interest, so unchanged
    /// interest needs no re-arm after every event.
    #[ntex::test]
    async fn level_mode_where_supported() {
        let fixture = Fixture::new();
        let mode = fixture.ops.0.mode;
        fixture.teardown();

        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(mode, PollMode::Level);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = mode;
    }

    /// A failed reactor operation stops the stream with its error.
    #[ntex::test]
    async fn reactor_error_stops_the_stream() {
        let mut fixture = Fixture::new();
        let id = fixture.id;
        fixture.handler.error(id, io::Error::other("reactor error"));
        // an unknown stream is ignored
        fixture.handler.error(id + 100, io::Error::other("unknown"));

        let closed = !fixture.io.is_active();
        fixture.teardown();
        assert!(closed, "reactor error did not stop the stream");
    }

    /// A new stream is registered with read interest armed, the first read
    /// request of the io task needs no interest update.
    #[ntex::test]
    async fn registration_arms_read_interest() {
        let fixture = Fixture::new();
        let flags = fixture.flags();
        fixture.teardown();
        assert!(flags.contains(Flags::RD), "read interest was not armed");
        assert!(!flags.contains(Flags::WR), "write interest was armed");
    }

    /// A read that would block keeps read interest armed.
    #[ntex::test]
    async fn readable_without_data_renews_read_interest() {
        let mut fixture = Fixture::new();
        fixture.fire(Event::readable(0));

        let flags = fixture.flags();
        fixture.teardown();
        assert!(flags.contains(Flags::RD), "read interest was not armed");
    }

    /// An event for the other direction keeps armed write interest.
    #[ntex::test]
    async fn readable_event_keeps_write_interest() {
        let mut fixture = Fixture::new();
        fixture
            .ops
            .0
            .with(|streams| streams[fixture.id].flags.insert(Flags::WR));
        fixture.fire(Event::readable(0));

        let flags = fixture.flags();
        fixture.teardown();
        assert!(flags.contains(Flags::WR), "write interest was lost");
    }

    /// Asking for write interest with leftover output arms it.
    #[ntex::test]
    async fn write_interest_with_pending_output() {
        let fixture = Fixture::new();
        // a write stops at the budget, so output is left over whatever the
        // socket accepts
        fixture
            .io
            .encode_slice(&vec![b'x'; WRITE_BUDGET * 2])
            .unwrap();

        fixture.ops.0.interest(fixture.id as u32, false, true);
        let armed = fixture.flags().contains(Flags::WR);
        // interest is already armed
        fixture.ops.0.interest(fixture.id as u32, false, true);
        let armed2 = fixture.flags().contains(Flags::WR);
        // write interest is dropped
        fixture.ops.0.interest(fixture.id as u32, false, false);
        let dropped = !fixture.flags().contains(Flags::WR);
        fixture.teardown();

        assert!(armed && armed2, "write interest was not armed");
        assert!(dropped, "write interest was not dropped");
    }

    /// Pausing reads leaves read interest armed. Resuming before a readable
    /// event needs neither an interest update nor a speculative read.
    #[ntex::test]
    async fn read_pause_keeps_interest_armed() {
        use std::io::Write;

        let mut fixture = Fixture::new();
        let id = fixture.id as u32;
        fixture.ops.0.interest(id, false, false);
        let paused = fixture.flags();
        fixture.peer.write_all(b"data").unwrap();
        fixture.ops.0.interest(id, true, false);
        let resumed = fixture.flags();
        let read = fixture.io.with_read_dst(|b| b.len());
        fixture.teardown();

        assert!(paused.contains(Flags::RD | Flags::RD_PAUSED), "{paused:?}");
        assert!(resumed.contains(Flags::RD), "{resumed:?}");
        assert!(!resumed.contains(Flags::RD_PAUSED), "{resumed:?}");
        assert_eq!(read, 0, "speculative read on resume");
    }

    /// Reads performed for a single readable event.
    mod read {
        use std::io::Write;

        use ntex_bytes::BytePageSize;
        use ntex_io::IoConfig;

        use super::*;

        /// Fills the stream's receive queue as far as the socket allows.
        fn fill(fixture: &mut Fixture) -> usize {
            let size = 2 * READ_BUDGET;
            fixture.ops.0.with(|streams| {
                streams[fixture.id].io.set_recv_buffer_size(size).unwrap();
            });
            let peer = socket2::SockRef::from(&fixture.peer);
            peer.set_send_buffer_size(size).unwrap();
            fixture.peer.set_nonblocking(true).unwrap();

            // Large writes: each one costs kernel overhead against the send
            // buffer, which Linux caps at `wmem_max`, 208 KB by default
            let chunk = vec![b'x'; 64 * 1024];
            let mut sent = 0;
            while sent < 4 * READ_BUDGET {
                match fixture.peer.write(&chunk) {
                    Ok(n) => sent += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
            sent
        }

        fn received(fixture: &Fixture) -> usize {
            fixture.io.with_read_dst(|b| b.len())
        }

        /// Reads continue past one filled chunk, but stop at `READ_BUDGET`.
        #[ntex::test]
        async fn read_continues_until_budget() {
            let max = BytePageSize::Size64;
            let mut fixture = Fixture::with_cfg(
                SharedCfg::new("READ").add(
                    IoConfig::default()
                        .set_read_size(BytePageSize::Size4, max)
                        .set_read_backpressure(8 * READ_BUDGET),
                ),
            );
            let sent = fill(&mut fixture);
            fixture.fire(Event::readable(0));

            let read = received(&fixture);
            fixture.teardown();

            assert!(
                sent > READ_BUDGET + max.capacity(),
                "socket too small: {sent}"
            );
            assert!(read > max.capacity(), "stopped after one chunk: {read}");
            // The last read may start just below the budget
            assert!(
                read < READ_BUDGET + 2 * max.capacity(),
                "budget exceeded: {read}"
            );
        }

        /// Read backpressure ends the reads of an event.
        #[ntex::test]
        async fn read_stops_on_backpressure() {
            let backpressure = BytePageSize::Size16.capacity();
            let mut fixture = Fixture::with_cfg(
                SharedCfg::new("READ").add(
                    IoConfig::default()
                        .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                        .set_read_backpressure(backpressure),
                ),
            );
            fill(&mut fixture);
            fixture.fire(Event::readable(0));

            let read = received(&fixture);
            let armed = fixture.flags().contains(Flags::RD);
            fixture.teardown();

            assert!(read >= backpressure, "stopped before backpressure: {read}");
            assert!(
                read < backpressure + BytePageSize::Size4.capacity(),
                "read past backpressure: {read}"
            );
            assert!(!armed, "read interest kept under backpressure");
        }

        /// Backpressure that drains the socket keeps read interest armed,
        /// the next readable event disarms it without reading.
        #[ntex::test]
        async fn drained_backpressure_keeps_interest_armed() {
            let mut fixture = Fixture::with_cfg(
                SharedCfg::new("READ").add(
                    IoConfig::default()
                        .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                        .set_read_backpressure(1024),
                ),
            );
            fixture.peer.write_all(&[b'x'; 2048]).unwrap();
            fixture.fire(Event::readable(0));
            let paused = fixture.flags();
            let read = received(&fixture);

            fixture.peer.write_all(&[b'x'; 2048]).unwrap();
            fixture.fire(Event::readable(0));
            let disarmed = fixture.flags();
            let read2 = received(&fixture);
            fixture.teardown();

            assert_eq!(read, 2048);
            assert!(paused.contains(Flags::RD | Flags::RD_PAUSED), "{paused:?}");
            assert!(
                !disarmed.intersects(Flags::RD | Flags::RD_PAUSED),
                "{disarmed:?}"
            );
            assert_eq!(read2, read, "read while paused");
        }
    }

    /// Out-of-band writes, performed on the stack of whoever filled the write
    /// buffer rather than by the write task.
    mod write {
        use super::*;

        /// More than any socket buffer holds, so output is guaranteed to be
        /// left over whatever the socket accepts.
        const LARGE: usize = 8 * 1024 * 1024;

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

        /// A write continues past one `writev` while the socket accepts
        /// everything, but stops at `WRITE_BUDGET`.
        #[ntex::test]
        async fn write_continues_until_budget() {
            use ntex_bytes::BytePageSize;
            use ntex_io::IoConfig;

            let page = BytePageSize::Size4;
            let fixture = Fixture::with_cfg(
                SharedCfg::new("WRITE").add(IoConfig::default().set_write_size(page)),
            );
            let size = 2 * WRITE_BUDGET;
            fixture.ops.0.with(|streams| {
                streams[fixture.id].io.set_send_buffer_size(size).unwrap();
            });
            socket2::SockRef::from(&fixture.peer)
                .set_recv_buffer_size(size)
                .unwrap();
            // Small pages cap a single `writev` at `MAX_WRITE_ITEMS` pages
            let chunk = [b'x'; 1024];
            for _ in 0..LARGE / chunk.len() {
                fixture.io.encode_slice(&chunk).unwrap();
            }

            fixture.ops.0.write_stream(fixture.id as u32);

            let written = LARGE - fixture.io.with_write_dst(|b| b.len());
            fixture.teardown();

            assert!(
                written > MAX_WRITE_ITEMS * page.capacity(),
                "stopped after one writev: {written}"
            );
            // The last `writev` may start just below the budget
            assert!(
                written < WRITE_BUDGET + MAX_WRITE_SIZE,
                "budget exceeded: {written}"
            );
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
