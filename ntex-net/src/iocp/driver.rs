use std::os::windows::io::{
    AsRawHandle, AsRawSocket, FromRawHandle, OwnedHandle, RawHandle,
};
use std::{cell::Cell, fmt, io, net, ptr, sync::Arc};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_MORE_DATA,
        ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE, NTSTATUS, RtlNtStatusToDosError,
        WAIT_TIMEOUT,
    },
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        Threading::INFINITE,
        WindowsProgramming::{
            FILE_SKIP_COMPLETION_PORT_ON_SUCCESS, FILE_SKIP_SET_EVENT_ON_HANDLE,
        },
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

    /// Driver turn is completed
    fn tick(&mut self) {}

    /// Clean up the handle before dropping the driver.
    fn cleanup(&mut self) {}
}

pub struct DriverApi {
    hnd: u32,
    reactor: Reactor,
}

impl DriverApi {
    /// Attach handle
    pub fn attach(&self, hnd: RawHandle) -> io::Result<()> {
        self.reactor.attach(hnd)
    }

    /// Get overlapped.
    pub fn overlapped(&self, id: u32) -> Overlapped {
        Overlapped::new(self.hnd, id)
    }

    #[inline]
    /// Attempt to cancel an already issued operation.
    pub fn cancel(&self, _h: RawHandle) {}
}

/// Low-level driver of io-uring.
pub struct Driver {
    hid: Cell<u32>,
    reactor: Reactor,
    #[allow(clippy::box_collection, clippy::type_complexity)]
    handlers: Cell<Option<Box<Vec<Box<dyn Handler>>>>>,
}

impl Driver {
    /// Create iocp driver
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            hid: Cell::new(0),
            reactor: Reactor::new()?,
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
        F: FnOnce(DriverApi) -> Box<dyn Handler>,
    {
        let hnd = self.hid.get() + 1;
        let mut handlers = self.handlers.take().unwrap_or_default();
        handlers.push(f(DriverApi {
            hnd,
            reactor: self.reactor.clone(),
        }));
        self.handlers.set(Some(handlers));
        self.hid.set(hnd);
    }
}

impl AsRawHandle for Driver {
    fn as_raw_handle(&self) -> RawHandle {
        self.reactor.0.port.as_raw_handle()
    }
}

impl crate::Reactor for Driver {
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
        self.reactor.attach(stream.as_raw_socket() as _)?;

        Ok(Io::new(
            TcpStream(Socket::from(stream), StreamOps::get(self)),
            cfg,
        ))
    }
}

impl ntex_rt::Driver for Driver {
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
                    self.reactor.0.port.as_raw_handle().cast(),
                    events.as_mut_ptr().cast(),
                    512,
                    &raw mut recv_count,
                    timeout,
                    0
                )
            );
            if let Err(err) = result {
                if err.raw_os_error() != Some(WAIT_TIMEOUT as _) {
                    return Err(err);
                }
            }

            self.poll_completions(&events[..recv_count as usize]);
        };

        for mut h in self.handlers.take().unwrap().into_iter() {
            h.cleanup();
        }
        result
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(self.reactor.handle())
    }
}

impl Driver {
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
                    | ERROR_NETNAME_DELETED
                    | ERROR_HANDLE_EOF
                    | ERROR_BROKEN_PIPE
                    | ERROR_PIPE_CONNECTED
                    | ERROR_PIPE_NOT_CONNECTED
                    | ERROR_NO_DATA
                    | ERROR_MORE_DATA => Ok(0),
                    _ => Err(io::Error::from_raw_os_error(error.cast_signed())),
                }
            };
            handlers[overlapped.hnd as usize].completed(
                overlapped.udata,
                result,
                overlapped_ptr,
            );
        }
        for hnd in handlers.iter_mut() {
            hnd.tick();
        }
        self.handlers.set(Some(handlers));
    }
}

#[derive(Clone, Debug)]
struct Reactor(Arc<ReactorInner>);

#[derive(Debug)]
struct ReactorInner {
    port: OwnedHandle,
    overlapped: Overlapped,
}

impl Reactor {
    fn new() -> io::Result<Self> {
        let port = unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1);
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(port)
        };
        log::trace!("New iocp reactor: {port:?}");

        Ok(Self(Arc::new(ReactorInner {
            port,
            overlapped: Overlapped::new(0, 0),
        })))
    }

    fn attach(&self, h: RawHandle) -> io::Result<()> {
        syscall!(
            BOOL,
            CreateIoCompletionPort(h, self.0.port.as_raw_handle(), 0, 0) as isize
        )?;
        syscall!(
            BOOL,
            SetFileCompletionNotificationModes(
                h,
                (FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE) as _
            )
        )?;
        Ok(())
    }

    fn handle(&self) -> ReactorHandle {
        ReactorHandle {
            inner: self.0.clone(),
        }
    }
}

#[derive(Clone, Debug)]
/// A notify handle to the driver.
pub(crate) struct ReactorHandle {
    inner: Arc<ReactorInner>,
}

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
                self.inner.overlapped.as_overlapped().cast()
            )
        )?;
        Ok(())
    }
}

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("hid", &self.hid)
            .field("reactor", &self.reactor)
            .finish()
    }
}

impl fmt::Debug for DriverApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverApi").field("hnd", &self.hnd).finish()
    }
}

struct Dummy;

impl Handler for Dummy {
    fn completed(&mut self, _: u32, _: io::Result<usize>, _: *mut Overlapped) {}

    fn cleanup(&mut self) {}
}
