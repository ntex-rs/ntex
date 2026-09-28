use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{cell::RefCell, io, mem, net, ptr, rc::Rc, task::Poll};

use ntex_io::Io;
use ntex_rt::{Arbiter, syscall};
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows_sys::{Win32::Networking::WinSock, core::GUID};

use super::{
    Handler, OpBox, Overlapped, OverlappedOp, Reactor, ReactorApi, TcpStream, UnixStream, ops,
    stream::StreamOps,
};
use crate::channel::{self, Receiver, Sender};

#[derive(Clone)]
pub(crate) struct ConnectOps(Rc<ConnectOpsInner>);

struct ConnectOpsHandler {
    inner: Rc<ConnectOpsInner>,
}

struct ConnectOp {
    overlapped: Overlapped,
    sock: Socket,
    addr: SockAddr,
    sender: Sender<Io>,
    cfg: SharedCfg,
}

// SAFETY: the pointer is derived from `this` by a place projection
unsafe impl OverlappedOp for ConnectOp {
    unsafe fn overlapped(this: *mut Self) -> *mut Overlapped {
        unsafe { &raw mut (*this).overlapped }
    }
}

type Operations = RefCell<Slab<OpBox<ConnectOp>>>;

struct ConnectOpsInner {
    api: ReactorApi,
    streams: StreamOps,
    ops: Operations,
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
        } else {
            Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "Unsupported address domain.",
            ))
        }
        .and_then(|baddr| sock.bind(&baddr))
        .and_then(|()| self.0.api.attach(sock.as_raw_socket() as _, true));

        if let Err(err) = result {
            Receiver::new(Err(err))
        } else {
            let mut ops = self.0.ops.borrow_mut();
            let entry = ops.vacant_entry();

            let (sender, rx) = channel::create();
            let op = OpBox::new(ConnectOp {
                overlapped: self.0.api.overlapped(entry.key() as u32),
                sock,
                addr,
                sender,
                cfg,
            });
            let mut sent = 0;
            let res = unsafe {
                self.0.connect.as_ref().unwrap()(
                    op.sock.as_raw_socket() as _,
                    op.addr.as_ptr().cast(),
                    op.addr.len(),
                    ptr::null(),
                    0,
                    &raw mut sent,
                    op.overlapped.as_overlapped(),
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
        match res.and_then(|()| update_connect_context(&self.sock)) {
            Ok(()) => {
                let io = if self.addr.domain() == Domain::UNIX {
                    Io::new(UnixStream(self.sock, self.addr, streams.clone()), self.cfg)
                } else {
                    Io::new(TcpStream(self.sock, self.addr, streams.clone()), self.cfg)
                };
                let _ = self.sender.send(Ok(io));
            }
            Err(err) => {
                let _ = self.sender.send(Err(err));
                crate::helpers::close_socket(self.sock);
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
        if let Some(op) = self.inner.ops.borrow_mut().try_remove(idx as usize) {
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: Connected({}) {res:?}",
                op.cfg.tag(),
                op.sock.as_raw_socket(),
            );

            op.into_inner()
                .complete(res.map(|_| ()), &self.inner.streams);
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
                    ptr::read(&raw const (*op).sock),
                    ptr::read(&raw const (*op).addr),
                    ptr::read(&raw const (*op).sender),
                    ptr::read(&raw const (*op).cfg),
                )
            };
            log::trace!(
                "{}: Cancel pending connect ({})",
                cfg.tag(),
                sock.as_raw_socket()
            );
            drop(sock);
            drop((addr, sender, cfg));
        }
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
    use std::os::windows::io::FromRawSocket;

    use super::*;

    /// Whether `io` is still an open socket bound to `addr`. Tests run in
    /// parallel, a closed handle value may already belong to another test.
    fn is_ours(io: RawSocket, addr: &SockAddr) -> bool {
        let s = mem::ManuallyDrop::new(unsafe { Socket::from_raw_socket(io) });
        s.local_addr().is_ok_and(|a| a == *addr)
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
