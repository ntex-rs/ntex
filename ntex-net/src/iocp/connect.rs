use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{cell::RefCell, io, mem, net, ptr, rc::Rc, task::Poll};

use ntex_io::Io;
use ntex_rt::{Arbiter, syscall};
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows_sys::{Win32::Networking::WinSock, core::GUID};

use super::{
    Handler, OpBox, Overlapped, Reactor, ReactorApi, TcpStream, UnixStream, ops, stream::StreamOps,
};
use crate::channel::{self, Receiver, Sender};
use crate::helpers::Queue;

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

struct ConnectOpsHandler {
    inner: Rc<ConnectOpsInner>,
}

struct ConnectOp {
    sock: Option<Socket>,
    addr: SockAddr,
    sender: Sender<Io>,
    cfg: SharedCfg,
}

type Operations = RefCell<Slab<OpBox<ConnectOp>>>;

struct ConnectOpsInner {
    api: ReactorApi,
    streams: StreamOps,
    ops: Operations,
    cancelled: Queue<usize>,
    connect: WinSock::LPFN_CONNECTEX,
}

impl ConnectOps {
    pub(crate) fn get(reactor: &Reactor) -> Self {
        let streams = StreamOps::get(reactor);

        Arbiter::get_value(move || {
            let mut inner = None;
            reactor.register(|api| {
                let dummy = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))
                    .expect("Cannot create socket");
                let connect = get_wsa_fn(dummy.as_raw_socket(), WinSock::WSAID_CONNECTEX)
                    .expect("Cannot get ConnectEx function");

                let ops = Rc::new(ConnectOpsInner {
                    api,
                    streams,
                    connect,
                    ops: RefCell::new(Slab::new()),
                    cancelled: Queue::new(),
                });
                inner = Some(ops.clone());
                Box::new(ConnectOpsHandler { inner: ops })
            });
            ConnectOps(inner.unwrap())
        })
    }

    pub(crate) fn connect(&self, sock: Socket, addr: SockAddr, cfg: SharedCfg) -> Receiver<Io> {
        let result = if addr.is_ipv4() {
            Ok(SockAddr::from(net::SocketAddrV4::new(
                net::Ipv4Addr::UNSPECIFIED,
                0,
            )))
        } else if addr.is_ipv6() {
            Ok(SockAddr::from(net::SocketAddrV6::new(
                net::Ipv6Addr::UNSPECIFIED,
                0,
                0,
                0,
            )))
        } else if addr.domain() == Domain::UNIX {
            // an unnamed address, `ConnectEx` requires a bound socket
            SockAddr::unix("")
        } else {
            Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Unsupported address domain.",
            ))
        }
        .and_then(|baddr| sock.bind(&baddr))
        .and_then(|()| {
            self.0
                .api
                .attach_stream(sock.as_raw_socket() as _, addr.domain())
        });

        if let Err(err) = result {
            Receiver::new(Err(err))
        } else {
            let mut ops = self.0.ops.borrow_mut();
            let entry = ops.vacant_entry();

            let idx = entry.key();
            let inner = Rc::downgrade(&self.0);
            let (sender, rx) = channel::create_with_cancel(move || {
                if let Some(inner) = inner.upgrade() {
                    inner.cancelled.push(idx);
                    if let Err(err) = inner.api.notify() {
                        log::error!("Cannot notify IOCP driver of cancelled connect: {err:?}");
                    }
                }
            });
            let op = OpBox::new(
                self.0.api.overlapped(idx as u32),
                ConnectOp {
                    sock: Some(sock),
                    addr,
                    sender,
                    cfg,
                },
            );
            let mut sent = 0;
            let res = unsafe {
                self.0.connect.as_ref().unwrap()(
                    op.sock.as_ref().unwrap().as_raw_socket() as _,
                    op.addr.as_ptr().cast(),
                    op.addr.len(),
                    ptr::null(),
                    0,
                    &raw mut sent,
                    op.as_overlapped(),
                )
            };

            match ops::win32_result(res) {
                Poll::Pending => {
                    entry.insert(op);
                }
                Poll::Ready(Ok(())) => op.into_inner().complete(Ok(()), &self.0.streams),
                Poll::Ready(Err(err)) => op.into_inner().complete(Err(err), &self.0.streams),
            }
            rx
        }
    }
}

impl ConnectOp {
    fn complete(self, res: io::Result<()>, streams: &StreamOps) {
        let Some(sock) = self.sock else {
            return;
        };
        let res = res.and_then(|()| update_connect_context(&sock));
        if self.sender.is_closed() {
            crate::helpers::close_socket(sock);
            return;
        }
        match res {
            Ok(()) => {
                let io = if self.addr.domain() == Domain::UNIX {
                    Io::new(UnixStream(sock, streams.clone()), self.cfg)
                } else {
                    let addr = self.addr.as_socket();
                    Io::new(TcpStream(sock, addr, streams.clone()), self.cfg)
                };
                let _ = self.sender.send(Ok(io));
            }
            Err(err) => {
                let _ = self.sender.send(Err(err));
                crate::helpers::close_socket(sock);
            }
        }
    }
}

/// Completes the connected state of a socket connected with `ConnectEx`.
///
/// Until this is set the socket is only partially connected: `shutdown`
/// fails with `WSAENOTCONN`, and socket options such as `SO_LINGER` do not
/// take effect on close. Without it a graceful close never sends a `FIN`
/// and leaks the socket, and a force close ends with a `FIN` instead of an
/// `RST`.
fn update_connect_context(sock: &Socket) -> io::Result<()> {
    syscall!(
        SOCKET,
        WinSock::setsockopt(
            sock.as_raw_socket() as _,
            WinSock::SOL_SOCKET,
            WinSock::SO_UPDATE_CONNECT_CONTEXT,
            ptr::null(),
            0,
        )
    )
    .map(|_| ())
}

impl Handler for ConnectOpsHandler {
    fn completed(&mut self, idx: u32, res: io::Result<usize>, _: *mut Overlapped) {
        let op = self.inner.ops.borrow_mut().try_remove(idx as usize);
        if let Some(op) = op {
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: Connected({:?}) {res:?}",
                op.cfg.tag(),
                op.sock.as_ref().map(AsRawSocket::as_raw_socket),
            );

            op.into_inner()
                .complete(res.map(|_| ()), &self.inner.streams);
        }
    }

    fn tick(&mut self) {
        while let Some(idx) = self.inner.cancelled.pop() {
            let sock = {
                let mut ops = self.inner.ops.borrow_mut();
                // A completion may have freed and reused the slot before this tick.
                ops.get_mut(idx)
                    .filter(|op| op.sender.is_closed())
                    .and_then(|op| op.sock.take())
            };
            // Closing cancels ConnectEx, but its allocation must stay alive
            // until the terminal completion is dequeued.
            drop(sock);
        }
    }

    fn cleanup(&mut self) {
        // The reactor has stopped, so the completions of pending `ConnectEx`
        // calls are never dequeued. Closing the socket cancels the connect,
        // but the kernel still writes its completion into the `OVERLAPPED`,
        // so the op allocation is leaked to keep that memory valid. The
        // other fields are dropped, which closes the socket and the channel.
        let ops = mem::take(&mut *self.inner.ops.borrow_mut());
        for (_, op) in ops {
            let op = op.into_raw();
            // SAFETY: each field is read once and the allocation is never
            // freed or dropped, so nothing is dropped twice
            let (sock, addr, sender, cfg) = unsafe {
                (
                    ptr::read(&raw const (*op).state.sock),
                    ptr::read(&raw const (*op).state.addr),
                    ptr::read(&raw const (*op).state.sender),
                    ptr::read(&raw const (*op).state.cfg),
                )
            };
            log::trace!(
                "{}: Cancel pending connect ({:?})",
                cfg.tag(),
                sock.as_ref().map(AsRawSocket::as_raw_socket)
            );
            drop(sock);
            drop((addr, sender, cfg));
        }
        self.inner.cancelled.clear();
    }
}

fn get_wsa_fn<F>(sock: RawSocket, fguid: GUID) -> io::Result<Option<F>> {
    let mut fptr = None;
    let mut returned = 0;
    syscall!(
        SOCKET,
        WinSock::WSAIoctl(
            sock as _,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            ptr::addr_of!(fguid).cast(),
            mem::size_of_val(&fguid) as _,
            ptr::addr_of_mut!(fptr).cast(),
            mem::size_of::<F>() as _,
            &raw mut returned,
            ptr::null_mut(),
            None,
        )
    )?;
    Ok(fptr)
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::io::FromRawSocket;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::IO::GetQueuedCompletionStatus;

    use super::*;

    /// Whether `io` is still an open socket bound to `addr`. Tests run in
    /// parallel, a closed handle value may already belong to another test.
    fn is_ours(io: RawSocket, addr: &SockAddr) -> bool {
        let s = mem::ManuallyDrop::new(unsafe { Socket::from_raw_socket(io) });
        s.local_addr().is_ok_and(|a| a == *addr)
    }

    fn pending_connect(
        ops: &ConnectOps,
        addr: net::SocketAddr,
    ) -> (Receiver<Io>, RawSocket, SockAddr) {
        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        let raw = sock.as_raw_socket();
        let rx = ops.connect(sock, addr.into(), SharedCfg::default());
        assert_eq!(ops.0.ops.borrow().len(), 1, "connect did not stay pending");
        let local = mem::ManuallyDrop::new(unsafe { Socket::from_raw_socket(raw) })
            .local_addr()
            .unwrap();
        (rx, raw, local)
    }

    fn drain_connects(reactor: &Reactor, ops: &ConnectOps) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ops.0.ops.borrow().is_empty() && Instant::now() < deadline {
            reactor.dispatch(10);
        }
        assert!(ops.0.ops.borrow().is_empty(), "connect completion was lost");
    }

    #[ntex::test]
    async fn dropped_connect_closes_socket_before_completion() {
        let reactor = Reactor::new().unwrap();
        let ops = ConnectOps::get(&reactor);
        let addr = net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (rx, raw, local) = pending_connect(&ops, addr);

        // Receiver destruction must not borrow the slab, including reentrant drops.
        let optr = {
            let entries = ops.0.ops.borrow_mut();
            let optr = entries.iter().next().unwrap().1.as_overlapped();
            drop(rx);
            assert!(is_ours(raw, &local));
            optr
        };

        // The cancellation wakes an otherwise idle reactor.
        reactor.dispatch(0);
        assert!(
            !is_ours(raw, &local),
            "cancelled connect still owns its socket"
        );
        {
            let entries = ops.0.ops.borrow();
            let op = entries.iter().next().unwrap().1;
            assert!(op.sock.is_none());
            assert_eq!(op.as_overlapped(), optr, "live operation storage was freed");
        }

        drain_connects(&reactor, &ops);
    }

    #[ntex::test]
    async fn abandoned_success_does_not_cancel_reused_slot() {
        let reactor = Reactor::new().unwrap();
        let ops = ConnectOps::get(&reactor);
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (rx, raw, local) = pending_connect(&ops, listener.local_addr().unwrap());
        let (idx, optr) = {
            let entries = ops.0.ops.borrow();
            let (idx, op) = entries.iter().next().unwrap();
            (idx, op.as_overlapped())
        };

        // Dequeue the real terminal completion, but drop the receiver before
        // delivering it, so the completion wins the race against cancellation.
        let (mut bytes, mut key, mut completed) = (0, 0, ptr::null_mut());
        syscall!(
            BOOL,
            GetQueuedCompletionStatus(
                reactor.as_raw_handle().cast(),
                &raw mut bytes,
                &raw mut key,
                &raw mut completed,
                5000,
            )
        )
        .unwrap();
        assert_eq!(key, 0);
        assert_eq!(completed, optr);
        drop(rx);
        let mut handler = ConnectOpsHandler {
            inner: ops.0.clone(),
        };
        handler.completed(idx as u32, Ok(bytes as usize), completed.cast());
        assert!(ops.0.ops.borrow().is_empty());
        assert!(!is_ours(raw, &local), "abandoned success registered an Io");

        let addr = net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (rx, raw, local) = pending_connect(&ops, addr);
        assert!(ops.0.ops.borrow().contains(idx), "slot was not reused");
        assert!(!ops.0.cancelled.is_empty());
        handler.tick();
        assert!(
            is_ours(raw, &local),
            "stale cancellation closed a new connect"
        );
        assert!(ops.0.ops.borrow()[idx].sock.is_some());

        drop(rx);
        handler.tick();
        assert!(!is_ours(raw, &local));
        drain_connects(&reactor, &ops);
    }

    /// A connect still in flight when the runtime stops must be cancelled:
    /// its socket is closed and the waiting receiver gets an error.
    #[ntex::test]
    async fn cleanup_cancels_pending_connect() {
        let reactor = Reactor::new().unwrap();
        let ops = ConnectOps::get(&reactor);

        // nothing listens on the port, a loopback connect is retried for a while
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        let raw = sock.as_raw_socket();
        let rx = ops.connect(sock, addr.into(), SharedCfg::default());
        assert_eq!(ops.0.ops.borrow().len(), 1, "connect did not stay pending");
        let local = mem::ManuallyDrop::new(unsafe { Socket::from_raw_socket(raw) })
            .local_addr()
            .unwrap();

        ConnectOpsHandler {
            inner: ops.0.clone(),
        }
        .cleanup();

        assert!(ops.0.ops.borrow().is_empty());
        assert!(!is_ours(raw, &local), "cleanup leaked the socket");
        let err = rx.await.unwrap_err();
        assert_eq!(err.to_string(), "IO Driver is gone");
    }
}
