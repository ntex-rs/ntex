use std::os::windows::io::{AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::{Arc, OnceLock, atomic::AtomicU8, atomic::Ordering};
use std::{cell::Cell, fmt, io, mem, net, ptr};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_CONNECTION_ABORTED, ERROR_CONNECTION_REFUSED, ERROR_HANDLE_EOF,
        ERROR_HOST_UNREACHABLE, ERROR_IO_INCOMPLETE, ERROR_MORE_DATA, ERROR_NETNAME_DELETED,
        ERROR_NETWORK_UNREACHABLE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        ERROR_PORT_UNREACHABLE, ERROR_SEM_TIMEOUT, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
        RtlNtStatusToDosError, WAIT_TIMEOUT,
    },
    Networking::WinSock,
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        LibraryLoader::{GetModuleHandleA, GetProcAddress},
        Threading::INFINITE,
        WindowsProgramming::{FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE},
    },
};

use ntex_io::Io;
use ntex_rt::{DriverType, Notify, PollResult, Runtime, syscall};
use ntex_service::cfg::SharedCfg;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use super::{Overlapped, TcpStream, connect, stream::StreamOps};
use crate::channel::Receiver;

pub(crate) trait Handler {
    /// Operation is completed.
    fn completed(&mut self, udata: u32, result: io::Result<usize>, optr: *mut Overlapped);

    /// Socket state notification for the stream `id`, see
    /// [`ReactorApi::register_notify()`].
    fn notified(&mut self, _id: usize, _events: u32) {}

    /// Reactor turn is completed
    fn tick(&mut self) {}

    /// Clean up the handle before dropping the driver.
    fn cleanup(&mut self) {}
}

/// IOCP reactor api.
pub(crate) struct ReactorApi {
    hnd: u32,
    reactor: Arc<ReactorInner>,
}

impl ReactorApi {
    /// Attach handle
    #[cfg(test)]
    pub(crate) fn attach(&self, hnd: RawHandle, skip_iocp_on_success: bool) -> io::Result<()> {
        self.reactor.attach(hnd, skip_iocp_on_success)
    }

    /// Attach stream socket of the `domain` with `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`
    pub(crate) fn attach_stream(&self, hnd: RawHandle, domain: Domain) -> io::Result<()> {
        self.reactor.attach_stream(hnd, domain)
    }

    /// Registers a stream socket for persistent edge-triggered read
    /// notifications, delivered to `Handler::notified()` with `id`.
    ///
    /// Returns `false` if socket state notifications are not available, or
    /// `id` does not fit into a completion key. The stream then waits for
    /// input with a zero-byte recv.
    pub(crate) fn register_notify(&self, sock: WinSock::SOCKET, id: usize) -> bool {
        notify_key(self.hnd as usize, id).is_some_and(|key| self.reactor.register_notify(sock, key))
    }

    /// Get overlapped.
    pub(crate) fn overlapped(&self, id: u32) -> Overlapped {
        Overlapped::new(self.hnd, id)
    }

    pub(crate) fn notify(&self) -> io::Result<()> {
        self.reactor.notify()
    }
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

    /// Registers a reactor handler.
    ///
    /// # Panics
    ///
    /// Panics if called during handler construction or completion dispatch,
    /// including notification and tick callbacks.
    pub(crate) fn register<F>(&self, f: F)
    where
        F: FnOnce(ReactorApi) -> Box<dyn Handler>,
    {
        let mut handlers = self
            .handlers
            .take()
            .expect("Cannot register an IOCP handler during registration or dispatch");
        let hnd = self.hid.get() + 1;
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
            .and_then(crate::helpers::prep_tcp_socket)
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
        stream.set_nodelay(true)?;
        let addr = stream.peer_addr()?;
        let domain = if addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
        self.reactor
            .attach_stream(stream.as_raw_socket() as _, domain)?;

        Ok(Io::new(
            TcpStream(Socket::from(stream), Some(addr), StreamOps::get(self)),
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
            // tasks woken until the runtime is polled do not need to notify
            rt.awake();

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
            // socket state notification, `lpOverlapped` is not a pointer
            if let Some((hnd, id)) = decode_notify_key(entry.lpCompletionKey) {
                if let Some(h) = handlers.get_mut(hnd) {
                    h.notified(id, entry.dwNumberOfBytesTransferred);
                }
                continue;
            }
            let overlapped_ptr: *mut Overlapped = entry.lpOverlapped.cast();
            // Copy completion data without borrowing the header: the handler
            // may resubmit the same op and let the kernel write it again.
            let hnd = unsafe { (*overlapped_ptr).hnd };
            if hnd == 0 {
                continue;
            }

            #[allow(clippy::cast_possible_wrap)]
            let (udata, status, size) = unsafe {
                (
                    (*overlapped_ptr).udata,
                    (*overlapped_ptr).base.Internal as NTSTATUS,
                    (*overlapped_ptr).base.InternalHigh,
                )
            };
            let result = if status >= 0 {
                Ok(size)
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
                    ERROR_MORE_DATA => Ok(size),
                    _ => Err(io::Error::from_raw_os_error(wsa_error(error))),
                }
            };
            handlers[hnd as usize].completed(udata, result, overlapped_ptr);
        }
        for hnd in handlers.iter_mut() {
            hnd.tick();
        }
        self.handlers.set(Some(handlers));
    }
}

#[cfg(test)]
impl Reactor {
    /// Handles the queued completions, waiting up to `timeout` milliseconds.
    pub(crate) fn dispatch(&self, timeout: u32) {
        let mut events = [OVERLAPPED_ENTRY::default(); 64];
        let mut count = 0;
        let result = syscall!(
            BOOL,
            GetQueuedCompletionStatusEx(
                self.reactor.port.as_raw_handle().cast(),
                events.as_mut_ptr().cast(),
                64,
                &raw mut count,
                timeout,
                0
            )
        );
        if result.is_ok() {
            self.poll_completions(&events[..count as usize]);
        }
    }
}

#[derive(Debug)]
struct ReactorInner {
    port: OwnedHandle,
    overlapped: Overlapped,
    /// Domains whose stream sockets passed the IFS check
    ifs: AtomicU8,
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
            ifs: AtomicU8::new(0),
        })
    }

    fn notify(&self) -> io::Result<()> {
        syscall!(
            BOOL,
            PostQueuedCompletionStatus(
                self.port.as_raw_handle().cast(),
                0,
                0,
                // the kernel does not write to the `OVERLAPPED` of a posted
                // packet, and the completion only reads the handler index
                (&raw const self.overlapped.base).cast_mut().cast()
            )
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn attach(&self, h: RawHandle, skip_iocp_on_success: bool) -> io::Result<()> {
        if skip_iocp_on_success {
            check_ifs_socket(h)?;
        }
        self.register(h, skip_iocp_on_success)
    }

    /// Attaches a stream socket, the IFS check runs once per domain.
    ///
    /// The provider of a socket is selected by its domain, type and protocol
    /// from the Winsock catalog, which does not change in practice while
    /// the process runs.
    fn attach_stream(&self, h: RawHandle, domain: Domain) -> io::Result<()> {
        let flag = ifs_flag(domain);
        if flag == 0 || self.ifs.load(Ordering::Relaxed) & flag == 0 {
            check_ifs_socket(h)?;
            self.ifs.fetch_or(flag, Ordering::Relaxed);
        }
        self.register(h, true)
    }

    fn register_notify(&self, sock: WinSock::SOCKET, key: usize) -> bool {
        let Some(psn) = process_socket_notifications() else {
            return false;
        };
        let mut reg = WinSock::SOCK_NOTIFY_REGISTRATION {
            socket: sock,
            completionKey: key as _,
            eventFilter: (WinSock::SOCK_NOTIFY_REGISTER_EVENT_IN
                | WinSock::SOCK_NOTIFY_REGISTER_EVENT_HANGUP) as u16,
            operation: WinSock::SOCK_NOTIFY_OP_ENABLE as u8,
            triggerFlags: (WinSock::SOCK_NOTIFY_TRIGGER_PERSISTENT
                | WinSock::SOCK_NOTIFY_TRIGGER_EDGE) as u8,
            registrationResult: 0,
        };
        let res = unsafe {
            psn(
                self.port.as_raw_handle(),
                1,
                &raw mut reg,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        res == 0 && reg.registrationResult == 0
    }

    fn register(&self, h: RawHandle, skip_iocp_on_success: bool) -> io::Result<()> {
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

/// Completion keys of socket state notifications have the top bit set, all
/// other packets use key 0. Below it are the handler index, then the stream
/// id in the low `NOTIFY_ID_BITS` bits: 15 and 48 bits on 64-bit targets,
/// 7 and 24 bits on 32-bit ones.
const NOTIFY_KEY: usize = 1 << (usize::BITS - 1);
const NOTIFY_ID_BITS: u32 = usize::BITS * 3 / 4;
const NOTIFY_ID_MASK: usize = (1 << NOTIFY_ID_BITS) - 1;
const NOTIFY_HND_LIMIT: usize = NOTIFY_KEY >> NOTIFY_ID_BITS;

fn notify_key(hnd: usize, id: usize) -> Option<usize> {
    (hnd < NOTIFY_HND_LIMIT && id <= NOTIFY_ID_MASK)
        .then_some(NOTIFY_KEY | hnd << NOTIFY_ID_BITS | id)
}

/// Returns the handler index and stream id of a notification packet's key.
pub(crate) fn decode_notify_key(key: usize) -> Option<(usize, usize)> {
    (key & NOTIFY_KEY != 0).then_some(((key & !NOTIFY_KEY) >> NOTIFY_ID_BITS, key & NOTIFY_ID_MASK))
}

type ProcessSocketNotificationsFn = unsafe extern "system" fn(
    HANDLE,
    u32,
    *mut WinSock::SOCK_NOTIFY_REGISTRATION,
    u32,
    u32,
    *mut OVERLAPPED_ENTRY,
    *mut u32,
) -> u32;

/// `ProcessSocketNotifications`, available since Windows build 20348 (Windows
/// Server 2022, Windows 11). It is looked up at runtime, a static import would
/// fail to load on older builds, which wait for input with zero-byte recvs.
fn process_socket_notifications() -> Option<ProcessSocketNotificationsFn> {
    static PSN: OnceLock<Option<ProcessSocketNotificationsFn>> = OnceLock::new();
    *PSN.get_or_init(|| unsafe {
        let module = GetModuleHandleA(c"ws2_32.dll".as_ptr().cast());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, c"ProcessSocketNotifications".as_ptr().cast()).map(|f| {
            mem::transmute::<unsafe extern "system" fn() -> isize, ProcessSocketNotificationsFn>(f)
        })
    })
}

fn ifs_flag(domain: Domain) -> u8 {
    if domain == Domain::IPV4 {
        1
    } else if domain == Domain::IPV6 {
        2
    } else if domain == Domain::UNIX {
        4
    } else {
        0
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
        self.inner.notify()
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
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::{Rc, Weak};

    use super::*;

    fn assert_registration_rejected(reactor: &Reactor) {
        let called = Cell::new(false);
        let id = reactor.hid.get();
        let result = catch_unwind(AssertUnwindSafe(|| {
            reactor.register(|_| {
                called.set(true);
                Box::new(Dummy)
            });
        }));
        assert!(result.is_err(), "reentrant registration was accepted");
        assert!(!called.get(), "reentrant handler factory was called");
        assert_eq!(reactor.hid.get(), id, "rejection changed the handler ID");
    }

    #[test]
    fn register_rejects_nested_factory() {
        let reactor = Reactor::new().unwrap();
        reactor.register(|api| {
            assert_eq!(api.hnd, 1);
            assert_registration_rejected(&reactor);
            Box::new(Dummy)
        });
        reactor.register(|api| {
            assert_eq!(api.hnd, 2);
            Box::new(Dummy)
        });
        let handlers = reactor.handlers.take().unwrap();
        assert_eq!(handlers.len(), 3);
        reactor.handlers.set(Some(handlers));
    }

    #[test]
    fn register_rejects_dispatch_callbacks() {
        struct Reentrant {
            reactor: Weak<Reactor>,
            calls: Rc<Cell<usize>>,
        }

        impl Reentrant {
            fn reject(&self) {
                assert_registration_rejected(&self.reactor.upgrade().unwrap());
                self.calls.set(self.calls.get() + 1);
            }
        }

        impl Handler for Reentrant {
            fn completed(&mut self, _: u32, _: io::Result<usize>, _: *mut Overlapped) {
                self.reject();
            }

            fn notified(&mut self, _: usize, _: u32) {
                self.reject();
            }

            fn tick(&mut self) {
                self.reject();
            }
        }

        let reactor = Rc::new(Reactor::new().unwrap());
        let calls = Rc::new(Cell::new(0));
        reactor.register(|_| {
            Box::new(Reentrant {
                reactor: Rc::downgrade(&reactor),
                calls: calls.clone(),
            })
        });
        let mut ov = Overlapped::new(1, 0);
        reactor.poll_completions(&[
            OVERLAPPED_ENTRY {
                lpOverlapped: &raw mut ov.base,
                ..OVERLAPPED_ENTRY::default()
            },
            OVERLAPPED_ENTRY {
                lpCompletionKey: notify_key(1, 0).unwrap(),
                ..OVERLAPPED_ENTRY::default()
            },
        ]);
        assert_eq!(calls.get(), 3);

        let done = Completions::default();
        let ticks = Rc::new(Cell::new(0));
        reactor.register(|api| {
            assert_eq!(api.hnd, 2);
            Box::new(Recorder(done.clone(), ticks.clone()))
        });
        let mut ov = Overlapped::new(2, 7);
        ov.base.InternalHigh = 11;
        reactor.poll_completions(&[OVERLAPPED_ENTRY {
            lpOverlapped: &raw mut ov.base,
            ..OVERLAPPED_ENTRY::default()
        }]);
        assert_eq!(*done.borrow(), [(7, Ok(11))]);
        assert_eq!(ticks.get(), 1);
    }

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

    type Completions = std::rc::Rc<std::cell::RefCell<Vec<(u32, Result<usize, io::ErrorKind>)>>>;

    struct Recorder(Completions, std::rc::Rc<Cell<usize>>);

    impl Handler for Recorder {
        fn completed(&mut self, udata: u32, result: io::Result<usize>, _: *mut Overlapped) {
            self.0
                .borrow_mut()
                .push((udata, result.map_err(|e| e.kind())));
        }

        fn tick(&mut self) {
            self.1.set(self.1.get() + 1);
        }
    }

    /// Completion statuses are translated into the operation result: pipe
    /// and eof statuses end the operation without an error, a partial
    /// transfer keeps its byte count, and notification packets are skipped.
    #[test]
    fn poll_completions_translates_status() {
        use windows_sys::Win32::Foundation as f;

        let reactor = Reactor::new().unwrap();
        let done = Completions::default();
        let ticks = std::rc::Rc::new(Cell::new(0));
        reactor.register(|_| Box::new(Recorder(done.clone(), ticks.clone())));

        let mut ops: Vec<Box<Overlapped>> = [
            (f::STATUS_SUCCESS, 7),
            (f::STATUS_PIPE_BROKEN, 7),
            (f::STATUS_END_OF_FILE, 7),
            (f::STATUS_BUFFER_OVERFLOW, 5),
            (f::STATUS_CONNECTION_REFUSED, 0),
            (f::STATUS_CANCELLED, 0),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (status, len))| {
            let mut ov = Box::new(Overlapped::new(1, u32::try_from(i).unwrap()));
            ov.base.Internal = status.cast_unsigned() as usize;
            ov.base.InternalHigh = len;
            ov
        })
        .collect();
        // a notification packet, it has no handler
        ops.push(Box::new(Overlapped::new(0, 99)));

        let events: Vec<_> = ops
            .iter_mut()
            .map(|ov| OVERLAPPED_ENTRY {
                lpOverlapped: (&raw mut ov.base),
                ..OVERLAPPED_ENTRY::default()
            })
            .collect();
        reactor.poll_completions(&events);

        assert_eq!(
            *done.borrow(),
            [
                (0, Ok(7)),
                (1, Ok(0)),
                (2, Ok(0)),
                (3, Ok(5)),
                (4, Err(io::ErrorKind::ConnectionRefused)),
                (
                    5,
                    Err(
                        io::Error::from_raw_os_error(f::ERROR_OPERATION_ABORTED.cast_signed())
                            .kind()
                    )
                ),
            ]
        );
        // one tick per batch of completions
        assert_eq!(ticks.get(), 1);
    }

    /// Sockets of the base Winsock providers return IFS handles and can be
    /// attached with `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`.
    #[test]
    fn attach_accepts_ifs_socket() {
        let reactor = ReactorInner::new().unwrap();
        for ty in [Type::STREAM, Type::DGRAM] {
            let sock = Socket::new(Domain::IPV4, ty, None).unwrap();
            check_ifs_socket(sock.as_raw_socket() as _).unwrap();
            reactor.attach(sock.as_raw_socket() as _, true).unwrap();
        }
    }

    /// The IFS check of stream sockets runs once per domain, failed checks
    /// are not cached.
    #[test]
    fn attach_stream_caches_ifs_check() {
        let reactor = ReactorInner::new().unwrap();
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(
            reactor
                .attach_stream(file.as_raw_handle(), Domain::IPV4)
                .is_err()
        );
        assert_eq!(reactor.ifs.load(Ordering::Relaxed), 0);

        for domain in [Domain::IPV4, Domain::IPV6] {
            for _ in 0..2 {
                let sock = Socket::new(domain, Type::STREAM, None).unwrap();
                reactor
                    .attach_stream(sock.as_raw_socket() as _, domain)
                    .unwrap();
            }
        }
        assert_eq!(reactor.ifs.load(Ordering::Relaxed), 3);
    }

    /// Accepted streams disable Nagle, as on the other backends.
    #[ntex::test]
    async fn from_tcp_stream_sets_nodelay() {
        use std::os::windows::io::FromRawSocket;

        let reactor = Reactor::new().unwrap();
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        assert!(!stream.nodelay().unwrap());

        let raw = stream.as_raw_socket();
        let io = crate::Reactor::from_tcp_stream(&reactor, stream, SharedCfg::default()).unwrap();
        let sock = mem::ManuallyDrop::new(unsafe { net::TcpStream::from_raw_socket(raw) });
        assert!(sock.nodelay().unwrap());
        drop(io);
    }

    /// Handles that are not sockets are rejected rather than attached
    /// without the check.
    #[test]
    fn check_ifs_rejects_non_socket() {
        let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(check_ifs_socket(file.as_raw_handle()).is_err());
    }
}
