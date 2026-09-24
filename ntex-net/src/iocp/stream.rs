use std::{cell::Cell, io, mem, os::windows::io::AsRawSocket, rc::Rc};

use ntex_io::IoContext;
use ntex_rt::{Arbiter, syscall};
use ntex_util::{channel::pool, future::Either};
use slab::Slab;
use socket2::{SockAddr, Socket};
use windows_sys::Win32::Networking::WinSock;

use super::{Handler, Overlapped, Reactor, ReactorApi, ops};
use crate::helpers::Queue;

/// Releases a socket, gracefully unless the connection was force-closed.
///
/// A graceful close drains the receive queue so that a pending `RST` cannot
/// destroy output that has not reached the peer yet, and shuts both directions
/// down before closing. A force close skips both and resets the connection
/// instead, so that the peer cannot mistake a truncated stream for a complete
/// one.
///
/// The socket is closed even if the graceful shutdown fails. The
/// shutdown error, if any, is reported in preference to a close error.
///
/// Callers other than `cleanup()`, which runs on the reactor thread, run this
/// on the blocking pool. That is safe for the drain: they only get here once
/// neither the recv nor the send is in flight, and no new recv can be started,
/// since only the connection task issues one and it is done by then.
fn close_socket(io: WinSock::SOCKET, terminate: bool) -> io::Result<()> {
    let shutdown = if terminate {
        crate::helpers::abort_raw_socket(io as _);
        Ok(())
    } else {
        crate::helpers::drain_raw_socket(io as _);
        syscall!(SOCKET, WinSock::shutdown(io, 2)).map(|_| ())
    };
    let close = syscall!(SOCKET, WinSock::closesocket(io)).map(|_| ());
    shutdown.and(close)
}

/// Releases a socket whose owner is gone, logging failures.
///
/// Used where there is nobody left to report the outcome to.
fn close_socket_detached(io: WinSock::SOCKET, terminate: bool, tag: &'static str) {
    ntex_rt::spawn_blocking(move || {
        if let Err(err) = close_socket(io, terminate) {
            log::error!("{tag}: Cannot close socket ({io:?}), {err:?}");
        }
    })
    .detach();
}

#[derive(Clone)]
pub(crate) struct StreamOps(Rc<StreamOpsInner>);

pub(crate) struct StreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

pub(crate) struct WeakStreamCtl {
    id: usize,
    inner: Rc<StreamOpsInner>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        /// the connection was force-closed, release it without a graceful close
        const TERMINATE    = 0b0001_0000;
        const CLOSED       = 0b0010_0000;
        const DROPPED_PRI  = 0b0100_0000;
        const DROPPED_SEC  = 0b1000_0000;
    }
}

#[derive(Debug)]
struct StreamItem {
    io: Socket,
    flags: Flags,
    addr: SockAddr,
    rd_op: ops::ReadOperation,
    wr_op: ops::WriteOperation,
    close: Option<pool::Sender<io::Result<()>>>,
}

struct StreamOpsHandler {
    inner: Rc<StreamOpsInner>,
}

enum IdType {
    Stream(u32),
    Weak(u32),
}

#[allow(clippy::box_collection)]
struct StreamOpsInner {
    api: ReactorApi,
    storage: Cell<Option<Box<StreamOpsStorage>>>,
    delayed_feed: Queue<IdType>,
    pool: pool::Pool<io::Result<()>>,
}

struct StreamOpsStorage {
    streams: Slab<Box<StreamItem>>,
}

impl StreamOps {
    /// Get `StreamOps` instance from the current runtime, or create new one
    pub(crate) fn get(reactor: &Reactor) -> Self {
        Arbiter::get_value(|| {
            let mut inner = None;
            reactor.register(|api| {
                let ops = Rc::new(StreamOpsInner {
                    api,
                    pool: pool::new(),
                    storage: Cell::new(Some(Box::new(StreamOpsStorage {
                        streams: Slab::new(),
                    }))),
                    delayed_feed: Queue::new(),
                });
                inner = Some(ops.clone());
                Box::new(StreamOpsHandler { inner: ops })
            });

            StreamOps(inner.unwrap())
        })
    }

    pub(crate) fn register(
        self,
        io: Socket,
        addr: SockAddr,
        ctx: IoContext,
    ) -> (StreamCtl, WeakStreamCtl) {
        let sock = io.as_raw_socket();
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: Registered({:?}) {:?}",
            ctx.tag(),
            sock,
            addr.as_socket()
        );

        let mut storage = self.0.storage.take().unwrap();
        let entry = storage.streams.vacant_entry();
        let id = entry.key();

        // read op
        let rd_op = ops::ReadOperation::new(id, sock, ctx.clone(), &self.0.api);

        // write op
        let wr_op = ops::WriteOperation::new(id, sock, ctx, &self.0.api);

        entry.insert(Box::new(StreamItem {
            io,
            rd_op,
            wr_op,
            addr,
            close: None,
            flags: Flags::empty(),
        }));
        self.0.storage.set(Some(storage));

        (
            StreamCtl {
                id,
                inner: self.0.clone(),
            },
            WeakStreamCtl {
                id,
                inner: self.0.clone(),
            },
        )
    }
}

impl Handler for StreamOpsHandler {
    fn completed(&mut self, udata: u32, res: io::Result<usize>, optr: *mut Overlapped) {
        if let Some(id) = match udata {
            ops::RD_OP => ops::ReadOperation::completed(res, optr),
            ops::WR_OP => ops::WriteOperation::completed(res, optr),
            _ => {
                log::warn!("Unknown operation: {udata}");
                None
            }
        } {
            self.inner.with(|st| {
                if let Some(item) = st.streams.get_mut(id)
                    && !item.flags.contains(Flags::CLOSED)
                    && item.rd_op.pause(true)
                    && item.wr_op.pause()
                    && let Some(tx) = item.close.take()
                {
                    item.flags.insert(Flags::CLOSED);
                    let _tag = item.rd_op.tag();
                    let io = item.io.as_raw_socket() as _;
                    #[cfg(feature = "trace")]
                    log::trace!("{_tag}: CloseWait({:?})", io);
                    let terminate = item.flags.contains(Flags::TERMINATE);
                    // Started right away rather than inside the task below: if
                    // the runtime stops before that task runs, the socket must
                    // still be closed, and `cleanup()` skips `CLOSED` items.
                    let close = ntex_rt::spawn_blocking(move || close_socket(io, terminate));
                    // `shutdown()` is waiting on `tx`, and reports the result
                    // through `IoContext::stopped`, so it must not resolve
                    // before the socket is actually closed
                    ntex_rt::spawn(async move {
                        let res = close.await.map_err(io::Error::other).and_then(|res| res);
                        #[cfg(feature = "trace")]
                        log::trace!("{_tag}: WaitClosed({:?})", io);
                        let _ = tx.send(res);
                    })
                    .detach();
                }
            });
        }
    }

    fn tick(&mut self) {
        self.inner.check_delayed_feed();
    }

    fn cleanup(&mut self) {
        // The reactor has stopped, so nothing will close the sockets that are
        // still registered: stored `IoContext`s keep `StreamOpsInner` alive
        // through the io handle, and the drop paths never run for them.
        if let Some(mut storage) = self.inner.storage.take() {
            for item in storage.streams.drain() {
                if item.flags.contains(Flags::CLOSED) {
                    // a close job owns the socket
                    mem::forget(item.io);
                    continue;
                }
                let io = item.io.as_raw_socket() as _;
                let tag = item.rd_op.tag();
                log::trace!(
                    "{tag}: Unclosed socket ({io:?}) {:?}",
                    item.addr.as_socket()
                );
                if let Err(err) = close_socket(io, item.flags.contains(Flags::TERMINATE)) {
                    log::error!("{tag}: Cannot close socket ({io:?}), {err:?}");
                }
                if item.rd_op.is_pending() || item.wr_op.is_pending() {
                    // Closing the socket cancels the pending operations, but
                    // the kernel still writes their completion into the
                    // `OVERLAPPED` and buffers inside the item, and nothing
                    // dequeues it anymore. Leak the item so that memory stays
                    // valid.
                    mem::forget(item);
                } else {
                    mem::forget(item.io);
                }
            }
            self.inner.storage.set(Some(storage));
        }
        self.inner.delayed_feed.clear();
    }
}

impl StreamOpsInner {
    fn with<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StreamOpsStorage) -> R,
    {
        let mut storage = self.storage.take().unwrap();
        let result = f(&mut storage);
        self.storage.set(Some(storage));
        result
    }

    fn check_delayed_feed(&self) {
        if !self.delayed_feed.is_empty()
            && let Some(mut storage) = self.storage.take()
        {
            while let Some(id) = self.delayed_feed.pop() {
                match id {
                    IdType::Stream(id) => storage.drop_stream(id as usize),
                    IdType::Weak(id) => storage.drop_weak_stream(id as usize),
                }
            }
            self.storage.set(Some(storage));
        }
    }
}

impl StreamCtl {
    pub(crate) async fn shutdown(&self, terminate: bool) -> io::Result<()> {
        let result = self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                if terminate {
                    // the connection was force-closed, so the deferred close
                    // below and the drop paths must skip the graceful close too
                    item.flags.insert(Flags::TERMINATE);
                }
                if item.flags.contains(Flags::CLOSED) {
                    None
                } else if item.rd_op.pause(true) && item.wr_op.pause() {
                    // no outstanding ops
                    item.flags.insert(Flags::CLOSED);
                    Some(Either::Left((
                        item.rd_op.tag(),
                        item.io.as_raw_socket() as _,
                    )))
                } else {
                    let (tx, rx) = self.inner.pool.channel();
                    item.close = Some(tx);
                    Some(Either::Right(rx))
                }
            } else {
                None
            }
        });

        match result {
            Some(Either::Left((_tag, io))) => {
                #[cfg(feature = "trace")]
                log::trace!("{_tag}: Close({io:?})");
                ntex_rt::spawn(ntex_rt::spawn_blocking(move || close_socket(io, terminate)))
                    .await
                    .map_err(io::Error::other)
                    .and_then(|res| res.map_err(io::Error::other))
                    .and_then(|res| res)
            }
            Some(Either::Right(rx)) => rx
                .await
                .map_err(|_| io::Error::other("Unexpected"))
                .and_then(|res| res),
            None => Ok(()),
        }
    }

    pub(crate) fn read(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.rd_op.read();
            }
        });
    }

    pub(crate) fn write(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.wr_op.write();
            }
        });
    }

    pub(crate) fn pause(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.rd_op.pause(false);
            }
        });
    }
}

impl StreamOpsStorage {
    fn drop_stream(&mut self, id: usize) {
        // Dropping while `StreamOps` handling event. The item is gone if
        // `cleanup()` already released it.
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: DropStream ({:?}) f:{:?}",
            item.rd_op.tag(),
            item.io.as_raw_socket(),
            item.flags,
        );

        if item.flags.contains(Flags::DROPPED_SEC) {
            Self::release(self.streams.remove(id));
        } else {
            item.flags.insert(Flags::DROPPED_PRI);
        }
    }

    fn drop_weak_stream(&mut self, id: usize) {
        // see `drop_stream`
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        #[cfg(feature = "trace")]
        log::trace!(
            "{}: DropStreamSec ({:?}) f:{:?}",
            item.rd_op.tag(),
            item.io.as_raw_socket(),
            item.flags,
        );

        if item.flags.contains(Flags::DROPPED_PRI) {
            Self::release(self.streams.remove(id));
        } else {
            item.flags.insert(Flags::DROPPED_SEC);
        }
    }

    /// Frees an item once both handles are gone, closing its socket unless a
    /// close job already owns it.
    fn release(item: Box<StreamItem>) {
        // The item holds the `OVERLAPPED` and buffers of its operations, so
        // freeing it while one is in flight lets the kernel complete into freed
        // memory. The connection task closes the socket, which waits for both
        // operations, before it drops its handle, and `cleanup()` takes the
        // items that are still registered at shutdown, so this cannot happen
        // today.
        debug_assert!(
            !item.rd_op.is_pending() && !item.wr_op.is_pending(),
            "{}: stream released with an operation in flight",
            item.rd_op.tag()
        );
        if !item.flags.contains(Flags::CLOSED) {
            let io = item.io.as_raw_socket() as _;
            let terminate = item.flags.contains(Flags::TERMINATE);
            close_socket_detached(io, terminate, item.rd_op.tag());
        }
        mem::forget(item.io);
    }
}

impl Drop for StreamCtl {
    fn drop(&mut self) {
        if let Some(mut storage) = self.inner.storage.take() {
            storage.drop_stream(self.id);
            self.inner.storage.set(Some(storage));
        } else {
            self.inner.delayed_feed.push(IdType::Stream(self.id as u32));
        }
    }
}

impl WeakStreamCtl {
    pub(crate) fn peer_addr(&self) -> SockAddr {
        self.inner.with(|st| st.streams[self.id].addr.clone())
    }

    pub(crate) fn write(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id) {
                item.wr_op.write();
            }
        });
    }
}

impl Drop for WeakStreamCtl {
    fn drop(&mut self) {
        if let Some(mut storage) = self.inner.storage.take() {
            storage.drop_weak_stream(self.id);
            self.inner.storage.set(Some(storage));
        } else {
            self.inner.delayed_feed.push(IdType::Weak(self.id as u32));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::{FromRawSocket, IntoRawSocket};

    use socket2::{Domain, Protocol, Type};

    use super::*;

    fn is_open(io: WinSock::SOCKET) -> bool {
        let mut val = 0i32;
        let mut len = i32::try_from(mem::size_of::<i32>()).unwrap();
        let res = unsafe {
            WinSock::getsockopt(
                io,
                WinSock::SOL_SOCKET,
                WinSock::SO_TYPE,
                (&raw mut val).cast(),
                &raw mut len,
            )
        };
        res == 0
    }

    /// Whether `io` is still the socket bound to `addr`. Tests run in
    /// parallel, a closed handle value may already belong to another test.
    fn is_ours(io: WinSock::SOCKET, addr: &socket2::SockAddr) -> bool {
        is_open(io) && {
            let s = mem::ManuallyDrop::new(unsafe { Socket::from_raw_socket(io as _) });
            s.local_addr().is_ok_and(|a| a == *addr)
        }
    }

    /// A failed graceful shutdown must not leave the socket open: the caller
    /// has already given up ownership of it, so nothing else would close it.
    #[test]
    fn close_socket_closes_when_shutdown_fails() {
        // `shutdown` fails with `WSAENOTCONN` on a socket that never connected
        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        sock.bind(
            &"127.0.0.1:0"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        )
        .unwrap();
        let addr = sock.local_addr().unwrap();
        let io = sock.into_raw_socket() as WinSock::SOCKET;

        let res = close_socket(io, false);
        let open = is_ours(io, &addr);
        if open {
            unsafe { WinSock::closesocket(io) };
        }

        assert_eq!(
            res.unwrap_err().raw_os_error(),
            Some(WinSock::WSAENOTCONN),
            "the shutdown error must still be reported"
        );
        assert!(!open, "socket leaked after a failed shutdown");
    }

    struct TestStream {
        socket: Socket,
        ops: StreamOps,
        ctl: Rc<Cell<Option<StreamCtl>>>,
    }

    struct TestHandle {
        _ctl: WeakStreamCtl,
    }

    impl ntex_io::Handle for TestHandle {
        fn query(&self, _: std::any::TypeId) -> Option<Box<dyn std::any::Any>> {
            None
        }
    }

    impl ntex_io::IoStream for TestStream {
        fn start(self, ctx: IoContext) -> Box<dyn ntex_io::Handle> {
            let addr = self.socket.peer_addr().unwrap();
            let (ctl, weak) = self.ops.register(self.socket, addr, ctx);
            self.ctl.set(Some(ctl));
            Box::new(TestHandle { _ctl: weak })
        }
    }

    /// Registers the client end of a loopback connection with a private
    /// reactor, and returns it along with the peer end.
    fn registered(
        reactor: &Reactor,
    ) -> (
        ntex_io::Io,
        StreamCtl,
        StreamOps,
        WinSock::SOCKET,
        std::net::TcpStream,
    ) {
        let lst = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(lst.local_addr().unwrap()).unwrap();
        let (peer, _) = lst.accept().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();

        let ops = StreamOps::get(reactor);
        let raw = client.as_raw_socket() as WinSock::SOCKET;
        ops.0.api.attach(raw as _, true).unwrap();
        let ctl = Rc::new(Cell::new(None));
        let io = ntex_io::Io::new(
            TestStream {
                socket: Socket::from(client),
                ops: ops.clone(),
                ctl: ctl.clone(),
            },
            ntex_service::cfg::SharedCfg::default(),
        );
        (io, ctl.take().unwrap(), ops, raw, peer)
    }

    fn cleanup(ops: &StreamOps) {
        StreamOpsHandler {
            inner: ops.0.clone(),
        }
        .cleanup();
    }

    /// Asserts the socket is closed, and releases it if it is not.
    fn assert_closed(io: WinSock::SOCKET, peer: &mut std::net::TcpStream) {
        let open = is_ours(io, &peer.peer_addr().unwrap().into());
        if open {
            unsafe { WinSock::closesocket(io) };
        }
        assert!(!open, "cleanup leaked the socket");
        let mut buf = [0u8; 16];
        assert_eq!(std::io::Read::read(peer, &mut buf).unwrap(), 0);
    }

    /// A socket that is still open when the runtime stops must be closed by
    /// `cleanup()`, and handles dropped afterwards must not touch it again.
    #[ntex::test]
    async fn cleanup_closes_idle_socket() {
        let reactor = Reactor::new().unwrap();
        let (io, ctl, ops, raw, mut peer) = registered(&reactor);

        cleanup(&ops);
        assert_closed(raw, &mut peer);

        drop(ctl);
        drop(io);
        cleanup(&ops);
    }

    /// A socket with a recv in flight must be closed too. The item stays
    /// allocated, since the kernel still completes the recv into it.
    #[ntex::test]
    async fn cleanup_closes_socket_with_pending_recv() {
        let reactor = Reactor::new().unwrap();
        let (io, ctl, ops, raw, mut peer) = registered(&reactor);
        ctl.read();
        assert!(
            ops.0.with(|st| st.streams[ctl.id].rd_op.is_pending()),
            "recv completed without data"
        );

        cleanup(&ops);
        assert_closed(raw, &mut peer);

        drop(ctl);
        drop(io);
    }
}
