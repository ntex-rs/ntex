use std::os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle, RawHandle};
use std::{cell::Cell, fmt, io, mem, net, ptr, sync::Arc};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_CONNECTION_ABORTED, ERROR_CONNECTION_REFUSED, ERROR_HANDLE_EOF,
        ERROR_HOST_UNREACHABLE, ERROR_IO_INCOMPLETE, ERROR_MORE_DATA, ERROR_NETNAME_DELETED,
        ERROR_NETWORK_UNREACHABLE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        ERROR_PORT_UNREACHABLE, ERROR_SEM_TIMEOUT, INVALID_HANDLE_VALUE, NTSTATUS,
        RtlNtStatusToDosError, WAIT_TIMEOUT,
    },
    Networking::WinSock,
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        Threading::INFINITE,
        WindowsProgramming::{FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE},
    },
};

use ntex_io::Io;
use ntex_rt::{DriverType, Notify, PollResult, Runtime, syscall};
use ntex_service::cfg::SharedCfg;
use socket2::{Protocol, SockAddr, Socket, Type};

use super::{Overlapped, TcpStream, connect, stream::StreamOps};
use crate::channel::Receiver;

pub trait Handler {
    /// Operation is completed.
    fn completed(&mut self, udata: u32, result: io::Result<usize>, optr: *mut Overlapped);

    /// Reactor turn is completed
    fn tick(&mut self) {}

    /// Clean up the handle before dropping the driver.
    fn cleanup(&mut self) {}
}

/// IOCP reactor api.
pub struct ReactorApi {
    hnd: u32,
    reactor: Arc<ReactorInner>,
}

impl ReactorApi {
    /// Attach handle
    pub fn attach(&self, hnd: RawHandle, skip_iocp_on_success: bool) -> io::Result<()> {
        self.reactor.attach(hnd, skip_iocp_on_success)
    }

    /// Get overlapped.
    pub fn overlapped(&self, id: u32) -> Overlapped {
        Overlapped::new(self.hnd, id)
    }

    #[inline]
    /// Attempt to cancel an already issued operation.
    pub fn cancel(&self, _h: RawHandle) {}
}

/// IOCP reactor.
pub struct Reactor {
    hid: Cell<u32>,
    #[allow(clippy::struct_field_names)]
    reactor: Arc<ReactorInner>,
    #[allow(clippy::box_collection, clippy::type_complexity)]
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Reactor {
    /// Create iocp driver
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            hid: Cell::new(0),
            reactor: Arc::new(ReactorInner::new()?),
            handlers: Cell::new(Some(Box::new(vec![Box::new(Dummy)]))),
        })
    }

    /// Driver type
    pub const fn tp(&self) -> DriverType {
        DriverType::Iocp
    }

    /// Register updates handler
    pub fn register<F>(&self, f: F)
    where
        F: FnOnce(ReactorApi) -> Box<dyn Handler>,
    {
        let hnd = self.hid.get() + 1;
        let mut handlers = self.handlers.take().unwrap_or_default();
        handlers.push(f(ReactorApi {
            hnd,
            reactor: self.reactor.clone(),
        }));
        self.handlers.set(Some(handlers));
        self.hid.set(hnd);
    }
}

impl AsRawHandle for Reactor {
    fn as_raw_handle(&self) -> RawHandle {
        self.reactor.port.as_raw_handle()
    }
}

impl crate::Reactor for Reactor {
    fn tcp_connect(&self, addr: net::SocketAddr, cfg: SharedCfg) -> Receiver<Io> {
        let addr = SockAddr::from(addr);
        let result = Socket::new(addr.domain(), Type::STREAM, Some(Protocol::TCP))
            .map(move |sock| (addr, sock));

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        let result = SockAddr::unix(addr).and_then(|addr| {
            Socket::new(addr.domain(), Type::STREAM, None).map(move |sock| (addr, sock))
        });

        match result {
            Err(err) => Receiver::new(Err(err)),
            Ok((addr, sock)) => connect::ConnectOps::get(self).connect(sock, addr, cfg),
        }
    }

    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
        let addr = stream.peer_addr()?;
        self.reactor.attach(stream.as_raw_socket() as _, true)?;

        Ok(Io::new(
            TcpStream(Socket::from(stream), addr.into(), StreamOps::get(self)),
            cfg,
        ))
    }
}

impl ntex_rt::Driver for Reactor {
    /// Poll the driver and handle completed operations.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        let mut events = [OVERLAPPED_ENTRY::default(); 512];
        let mut recv_count = 0;

        let result = loop {
            let timeout = match rt.poll() {
                PollResult::Pending => INFINITE,
                PollResult::PollAgain => 0,
                PollResult::Ready => break Ok(()),
            };

            let result = syscall!(
                BOOL,
                GetQueuedCompletionStatusEx(
                    self.reactor.port.as_raw_handle().cast(),
                    events.as_mut_ptr().cast(),
                    512,
                    &raw mut recv_count,
                    timeout,
                    0
                )
            );

            match result {
                Err(err) => {
                    if err.raw_os_error() != Some(WAIT_TIMEOUT.cast_signed()) {
                        break Err(err);
                    }
                }
                Ok(_) => self.poll_completions(&events[..recv_count as usize]),
            }
        };

        for mut h in self.handlers.take().unwrap().into_iter() {
            h.cleanup();
        }
        result
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(ReactorHandle {
            inner: self.reactor.clone(),
        })
    }
}

/// Maps a Win32 error of a socket completion to its `WinSock` error.
///
/// `RtlNtStatusToDosError` maps socket NTSTATUS codes to Win32 network errors,
/// such as `ERROR_CONNECTION_REFUSED`, which `io::Error::kind()` does not
/// classify. `WSAGetOverlappedResult` reports `WinSock` errors instead.
fn wsa_error(error: u32) -> i32 {
    match error {
        ERROR_CONNECTION_REFUSED => WinSock::WSAECONNREFUSED,
        ERROR_NETNAME_DELETED | ERROR_PORT_UNREACHABLE => WinSock::WSAECONNRESET,
        ERROR_CONNECTION_ABORTED => WinSock::WSAECONNABORTED,
        ERROR_NETWORK_UNREACHABLE => WinSock::WSAENETUNREACH,
        ERROR_HOST_UNREACHABLE => WinSock::WSAEHOSTUNREACH,
        ERROR_SEM_TIMEOUT => WinSock::WSAETIMEDOUT,
        _ => error.cast_signed(),
    }
}

/// Maps a Win32 network error of a socket completion to its `WinSock` error,
/// so that `io::Error::kind()` classifies it.
#[cfg_attr(not(feature = "compio"), allow(dead_code))]
pub(crate) fn map_socket_error(err: io::Error) -> io::Error {
    match err.raw_os_error() {
        Some(code) => {
            let mapped = wsa_error(code.cast_unsigned());
            if mapped == code {
                err
            } else {
                io::Error::from_raw_os_error(mapped)
            }
        }
        None => err,
    }
}

impl Reactor {
    /// Handle ring completions, forward changes to specific handler
    fn poll_completions(&self, events: &[OVERLAPPED_ENTRY]) {
        let mut handlers = self.handlers.take().unwrap();
        for entry in events {
            let overlapped_ptr: *mut Overlapped = entry.lpOverlapped.cast();
            let overlapped = unsafe { &*overlapped_ptr };
            if overlapped.hnd == 0 {
                continue;
            }

            #[allow(clippy::cast_possible_wrap)]
            let status = overlapped.base.Internal as NTSTATUS;
            let result = if status >= 0 {
                Ok(overlapped.base.InternalHigh)
            } else {
                let error = unsafe { RtlNtStatusToDosError(status) };
                match error {
                    ERROR_IO_INCOMPLETE
                    | ERROR_HANDLE_EOF
                    | ERROR_BROKEN_PIPE
                    | ERROR_PIPE_CONNECTED
                    | ERROR_PIPE_NOT_CONNECTED
                    | ERROR_NO_DATA => Ok(0),
                    // Partial transfer: data was delivered and more remains, so
                    // reporting 0 here would be read as a clean eof / write-zero.
                    ERROR_MORE_DATA => Ok(overlapped.base.InternalHigh),
                    _ => Err(io::Error::from_raw_os_error(wsa_error(error))),
                }
            };
            handlers[overlapped.hnd as usize].completed(overlapped.udata, result, overlapped_ptr);
        }
        for hnd in handlers.iter_mut() {
            hnd.tick();
        }
        self.handlers.set(Some(handlers));
    }
}

#[derive(Debug)]
struct ReactorInner {
    port: OwnedHandle,
    overlapped: Overlapped,
}

impl ReactorInner {
    fn new() -> io::Result<Self> {
        let port = unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1);
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(port)
        };
        log::trace!("New iocp reactor: {port:?}");

        Ok(ReactorInner {
            port,
            overlapped: Overlapped::new(0, 0),
        })
    }

    fn attach(&self, h: RawHandle, skip_iocp_on_success: bool) -> io::Result<()> {
        if skip_iocp_on_success {
            check_ifs_socket(h)?;
        }
        syscall!(
            BOOL,
            CreateIoCompletionPort(h, self.port.as_raw_handle(), 0, 0) as isize
        )?;
        if skip_iocp_on_success {
            syscall!(
                BOOL,
                SetFileCompletionNotificationModes(
                    h,
                    (FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE) as _
                )
            )?;
        }
        Ok(())
    }
}

/// Fails for sockets whose provider does not return IFS handles.
///
/// A non-IFS layered service provider (LSP) can post a completion packet for
/// an operation that completed synchronously, even with
/// `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS` set. The reactor already handles
/// such operations inline, so the extra packet would complete the slot a
/// second time, possibly after it was reused by another operation.
fn check_ifs_socket(h: RawHandle) -> io::Result<()> {
    let mut info: WinSock::WSAPROTOCOL_INFOW = unsafe { mem::zeroed() };
    let mut len = i32::try_from(mem::size_of_val(&info)).unwrap();
    syscall!(
        SOCKET,
        WinSock::getsockopt(
            h as _,
            WinSock::SOL_SOCKET,
            WinSock::SO_PROTOCOL_INFOW,
            (&raw mut info).cast(),
            &raw mut len,
        )
    )?;
    if info.dwServiceFlags1 & WinSock::XP1_IFS_HANDLES == 0 {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Socket provider does not support IFS handles, non-IFS LSPs are not supported",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
/// A notify handle to the driver.
pub(crate) struct ReactorHandle {
    inner: Arc<ReactorInner>,
}

/// SAFETY: `ReactorInner` holds iocp port handle which is thread safe
unsafe impl Send for ReactorInner {}
unsafe impl Sync for ReactorInner {}

impl Notify for ReactorHandle {
    /// Notify the driver.
    fn notify(&self) -> io::Result<()> {
        syscall!(
            BOOL,
            PostQueuedCompletionStatus(
                self.inner.port.as_raw_handle().cast(),
                0,
                0,
                // the kernel does not write to the `OVERLAPPED` of a posted
                // packet, and the completion only reads the handler index
                (&raw const self.inner.overlapped.base).cast_mut().cast()
            )
        )?;
        Ok(())
    }
}

impl fmt::Debug for Reactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reactor")
            .field("hid", &self.hid)
            .field("reactor", &self.reactor)
            .finish()
    }
}

impl fmt::Debug for ReactorApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReactorApi")
            .field("hnd", &self.hnd)
            .finish()
    }
}

struct Dummy;

impl Handler for Dummy {
    fn completed(&mut self, _: u32, _: io::Result<usize>, _: *mut Overlapped) {}

    fn cleanup(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Socket completion errors have the same kind as synchronous socket errors.
    #[test]
    fn socket_completion_error_kind() {
        use windows_sys::Win32::Foundation as f;
        for (status, kind) in [
            (
                f::STATUS_CONNECTION_REFUSED,
                io::ErrorKind::ConnectionRefused,
            ),
            (f::STATUS_CONNECTION_RESET, io::ErrorKind::ConnectionReset),
            (f::STATUS_REMOTE_DISCONNECT, io::ErrorKind::ConnectionReset),
            (
                f::STATUS_CONNECTION_ABORTED,
                io::ErrorKind::ConnectionAborted,
            ),
            (
                f::STATUS_NETWORK_UNREACHABLE,
                io::ErrorKind::NetworkUnreachable,
            ),
            (f::STATUS_HOST_UNREACHABLE, io::ErrorKind::HostUnreachable),
            (f::STATUS_IO_TIMEOUT, io::ErrorKind::TimedOut),
        ] {
            let error = unsafe { RtlNtStatusToDosError(status) };
            let err = io::Error::from_raw_os_error(wsa_error(error));
            assert_eq!(err.kind(), kind, "{status:#x}: {err:?}");

            let err = map_socket_error(io::Error::from_raw_os_error(error.cast_signed()));
            assert_eq!(err.kind(), kind, "{status:#x}: {err:?}");
        }
    }

    /// Sockets of the base Winsock providers return IFS handles and can be
    /// attached with `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`.
    #[test]
    fn attach_accepts_ifs_socket() {
        let reactor = ReactorInner::new().unwrap();
        for ty in [Type::STREAM, Type::DGRAM] {
            let sock = Socket::new(socket2::Domain::IPV4, ty, None).unwrap();
            check_ifs_socket(sock.as_raw_socket() as _).unwrap();
            reactor.attach(sock.as_raw_socket() as _, true).unwrap();
        }
    }

    /// Handles that are not sockets are rejected rather than attached
    /// without the check.
    #[test]
    fn check_ifs_rejects_non_socket() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(check_ifs_socket(file.as_raw_handle()).is_err());
    }
}
