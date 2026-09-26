use std::{cell::Cell, io, mem, os::windows::io::AsRawSocket, pin::pin, rc::Rc};

use ntex_io::IoContext;
use ntex_rt::{Arbiter, syscall};
use ntex_util::future::{Either, select};
use ntex_util::{channel::pool, time::sleep};
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
    // Boxed so that a reference to the item does not cover them: a recv that
    // completes immediately runs the read filters inside `ReadOperation::read()`,
    // and a write they issue borrows the item while that `&mut` is live.
    rd_op: Box<ops::ReadOperation>,
    wr_op: Box<ops::WriteOperation>,
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
        let rd_op = Box::new(ops::ReadOperation::new(id, sock, ctx.clone(), &self.0.api));

        // write op
        let wr_op = Box::new(ops::WriteOperation::new(id, sock, ctx, &self.0.api));

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
                    && item.pause_ops()
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
                } else {
                    // a forced close does not wait for the completions, the
                    // item is freed once the last one arrives
                    st.release_if_done(id);
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
                    // a close job owns the socket, a forced close may have left
                    // operations in flight, see below
                    if item.rd_op.is_pending() || item.wr_op.is_pending() {
                        mem::forget(item);
                    } else {
                        mem::forget(item.io);
                    }
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
                } else if item.pause_ops() {
                    // no outstanding ops
                    item.flags.insert(Flags::CLOSED);
                    Some(Either::Left((
                        item.rd_op.tag(),
                        item.io.as_raw_socket() as _,
                    )))
                } else {
                    let (tx, rx) = self.inner.pool.channel();
                    item.close = Some(tx);
                    Some(Either::Right((rx, item.rd_op.shutdown_timeout())))
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
            Some(Either::Right((rx, timeout))) => {
                let mut rx = pin!(rx);
                if let Either::Left(res) = select(rx.as_mut(), sleep(timeout)).await {
                    return res
                        .map_err(|_| io::Error::other("Unexpected"))
                        .and_then(|res| res);
                }
                // A cancelled operation has not completed in time, it may never
                // do. Closing the socket completes it, unless the close has
                // started meanwhile.
                let Some((tag, io)) = self.inner.with(|st| st.force_close(self.id)) else {
                    return rx
                        .await
                        .map_err(|_| io::Error::other("Unexpected"))
                        .and_then(|res| res);
                };
                log::warn!(
                    "{tag}: Cancelled operations did not complete in {timeout:?}, closing socket ({io:?})"
                );
                let res = ntex_rt::spawn(ntex_rt::spawn_blocking(move || close_socket(io, true)))
                    .await
                    .map_err(io::Error::other)
                    .and_then(|res| res.map_err(io::Error::other))
                    .and_then(|res| res);
                if let Err(err) = res {
                    log::error!("{tag}: Cannot close socket ({io:?}), {err:?}");
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cancelled operations did not complete",
                ))
            }
            None => Ok(()),
        }
    }

    pub(crate) fn read(&self) {
        let op = self.inner.with(|st| {
            st.streams
                .get_mut(self.id)
                .filter(|item| !item.is_closing())
                .map(|item| &raw mut *item.rd_op)
        });
        if let Some(op) = op {
            // Issued outside `with()`: a recv that completes immediately runs
            // the read filters, and output they produce may be written right
            // away through `WeakStreamCtl::write`, which needs the storage.
            //
            // SAFETY: the operation is boxed, so its address is stable, and it
            // is only freed once this handle is dropped or the reactor stops. A
            // close only starts from `shutdown()`, never from within the read.
            // A write issued from within the read borrows the item, which does
            // not overlap the boxed operation.
            unsafe { (*op).read() };
        }
    }

    pub(crate) fn write(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id)
                && !item.is_closing()
            {
                item.wr_op.write();
            }
        });
    }

    pub(crate) fn pause(&self) {
        self.inner.with(|st| {
            if let Some(item) = st.streams.get_mut(self.id)
                && !item.is_closing()
            {
                item.rd_op.pause(false);
            }
        });
    }
}

impl StreamItem {
    /// Cancels the operations in flight, and marks them as closing so their
    /// completions start the close. Returns `true` if none is in flight.
    ///
    /// Both are always paused: a send left running would issue the next one
    /// once it completes.
    fn pause_ops(&mut self) -> bool {
        let rd = self.rd_op.pause(true);
        let wr = self.wr_op.pause();
        rd && wr
    }

    /// Whether the close has started. No operation may be started then, and
    /// once the socket is closed its handle value may belong to another one.
    fn is_closing(&self) -> bool {
        self.close.is_some() || self.flags.contains(Flags::CLOSED)
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

        item.flags.insert(Flags::DROPPED_PRI);
        self.release_if_done(id);
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

        item.flags.insert(Flags::DROPPED_SEC);
        self.release_if_done(id);
    }

    /// Frees an item once both handles are gone and no operation is in flight.
    ///
    /// Operations can still be in flight after a forced close, the completion
    /// of the last one calls this again.
    fn release_if_done(&mut self, id: usize) {
        let Some(item) = self.streams.get_mut(id) else {
            return;
        };
        if !item.flags.contains(Flags::DROPPED_PRI | Flags::DROPPED_SEC) {
            return;
        }
        if item.rd_op.is_pending() || item.wr_op.is_pending() {
            if !item.flags.contains(Flags::CLOSED) {
                // dropped without a shutdown, cancel so that the completions
                // arrive; once closed, the handle value may be reused
                item.rd_op.pause(true);
                item.wr_op.pause();
            }
            return;
        }
        Self::release(*self.streams.remove(id));
    }

    /// Stops waiting for cancelled operations and takes over the close.
    ///
    /// Returns the socket to close, or `None` if the close has already started
    /// because the operations completed meanwhile.
    fn force_close(&mut self, id: usize) -> Option<(&'static str, WinSock::SOCKET)> {
        let item = self.streams.get_mut(id)?;
        item.close.take()?;
        item.flags.insert(Flags::CLOSED | Flags::TERMINATE);
        Some((item.rd_op.tag(), item.io.as_raw_socket() as _))
    }

    /// Frees an item once both handles are gone, closing its socket unless a
    /// close job already owns it.
    fn release(item: StreamItem) {
        // The item holds the `OVERLAPPED` and buffers of its operations, so
        // freeing it while one is in flight lets the kernel complete into freed
        // memory. `release_if_done()` waits for both operations, and
        // `cleanup()` takes the items that are still registered at shutdown.
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
            if let Some(item) = st.streams.get_mut(self.id)
                && !item.is_closing()
            {
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

    /// Delivers an aborted completion for a recv marked by `fake_pending()`.
    fn complete_aborted_recv(ops: &StreamOps, id: usize) {
        let optr = ops
            .0
            .with(|st| (&raw mut *st.streams[id].rd_op).cast::<Overlapped>());
        complete_aborted(ops, ops::RD_OP, optr);
    }

    /// Delivers an aborted completion for a send marked by `fake_pending()`.
    fn complete_aborted_send(ops: &StreamOps, id: usize) {
        let optr = ops
            .0
            .with(|st| (&raw mut *st.streams[id].wr_op).cast::<Overlapped>());
        complete_aborted(ops, ops::WR_OP, optr);
    }

    fn complete_aborted(ops: &StreamOps, udata: u32, optr: *mut Overlapped) {
        let err = io::Error::from_raw_os_error(
            i32::try_from(windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED).unwrap(),
        );
        StreamOpsHandler {
            inner: ops.0.clone(),
        }
        .completed(udata, Err(err), optr);
    }

    /// A shutdown with both a recv and a send in flight must cancel both. A
    /// send left running would start the next one once it completes, and a
    /// stalled one would hold up the close until the shutdown timeout.
    #[ntex::test]
    async fn shutdown_cancels_pending_recv_and_send() {
        let reactor = Reactor::new().unwrap();
        let (io, ctl, ops, raw, mut peer) = registered(&reactor);
        let id = ctl.id;
        ops.0.with(|st| {
            st.streams[id].rd_op.fake_pending();
            st.streams[id].wr_op.fake_pending();
        });

        {
            let mut fut = pin!(ntex::time::timeout(
                ntex::time::Seconds(5),
                ctl.shutdown(false)
            ));
            std::future::poll_fn(|cx| {
                assert!(fut.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(
                ops.0.with(|st| st.streams[id].wr_op.is_closing()),
                "send in flight not cancelled"
            );

            complete_aborted_send(&ops, id);
            complete_aborted_recv(&ops, id);
            fut.await
                .expect("close did not start after both completions")
                .unwrap();
        }
        assert_closed(raw, &mut peer);

        drop(ctl);
        drop(io);
    }

    /// A close waiting for a cancelled recv that never completes must give up
    /// after the shutdown timeout and close the socket, resetting the
    /// connection. The item stays allocated until the recv does complete.
    #[ntex::test]
    async fn shutdown_closes_socket_when_cancel_does_not_complete() {
        let reactor = Reactor::new().unwrap();
        let (io, ctl, ops, raw, mut peer) = registered(&reactor);
        let id = ctl.id;
        ops.0.with(|st| st.streams[id].rd_op.fake_pending());

        let err = ntex::time::timeout(ntex::time::Seconds(5), ctl.shutdown(false))
            .await
            .expect("close waited for the recv past the shutdown timeout")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        let open = is_ours(raw, &peer.peer_addr().unwrap().into());
        if open {
            unsafe { WinSock::closesocket(raw) };
        }
        assert!(!open, "socket not closed");
        let mut buf = [0u8; 16];
        let err = std::io::Read::read(&mut peer, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);

        drop(ctl);
        drop(io);
        assert!(
            ops.0.with(|st| st.streams.contains(id)),
            "item freed with a recv in flight"
        );
        complete_aborted_recv(&ops, id);
        assert!(
            !ops.0.with(|st| st.streams.contains(id)),
            "item not freed after the completion"
        );
    }
}
