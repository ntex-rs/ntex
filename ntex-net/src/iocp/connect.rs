use std::os::windows::io::{AsRawSocket, RawSocket};
use std::{cell::RefCell, io, mem, net, ptr, rc::Rc, task::Poll};

use ntex_io::Io;
use ntex_rt::{Arbiter, syscall};
use ntex_service::cfg::SharedCfg;
use slab::Slab;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows_sys::{Win32::Networking::WinSock, core::GUID};

use super::{
    Driver, DriverApi, Handler, Overlapped, TcpStream, UnixStream, ops, stream::StreamOps,
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

type Operations = RefCell<Slab<Box<ConnectOp>>>;

struct ConnectOpsInner {
    api: DriverApi,
    streams: StreamOps,
    ops: Operations,
    connect: WinSock::LPFN_CONNECTEX,
}

impl ConnectOps {
    pub(crate) fn get(driver: &Driver) -> Self {
        let streams = StreamOps::get(driver);

        Arbiter::get_value(move || {
            let mut inner = None;
            driver.register(|api| {
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

    pub(crate) fn connect(
        &self,
        sock: Socket,
        addr: SockAddr,
        cfg: SharedCfg,
    ) -> Receiver<Io> {
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
            let op = Box::new(ConnectOp {
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
                Poll::Ready(Ok(())) => {
                    if op.addr.domain() == Domain::UNIX {
                        let _ = op.sender.send(Ok(Io::new(
                            UnixStream(op.sock, op.addr, self.0.streams.clone()),
                            op.cfg,
                        )));
                    } else {
                        let _ = op.sender.send(Ok(Io::new(
                            TcpStream(op.sock, op.addr, self.0.streams.clone()),
                            op.cfg,
                        )));
                    }
                }
                Poll::Ready(Err(err)) => {
                    let _ = op.sender.send(Err(err));
                    crate::helpers::close_socket(op.sock);
                }
            }
            rx
        }
    }
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

            match res {
                Ok(_) => {
                    if op.addr.domain() == Domain::UNIX {
                        let _ = op.sender.send(Ok(Io::new(
                            UnixStream(op.sock, op.addr, self.inner.streams.clone()),
                            op.cfg,
                        )));
                    } else {
                        let _ = op.sender.send(Ok(Io::new(
                            TcpStream(op.sock, op.addr, self.inner.streams.clone()),
                            op.cfg,
                        )));
                    }
                }
                Err(err) => {
                    let _ = op.sender.send(Err(err));
                    crate::helpers::close_socket(op.sock);
                }
            }
        }
    }

    fn cleanup(&mut self) {}
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
