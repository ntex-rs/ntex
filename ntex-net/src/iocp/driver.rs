use std::cell::{Cell, UnsafeCell};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
// use std::os::windows::net::UnixStream as OsUnixStream;
use std::{cmp, collections::VecDeque, fmt, io, mem, net, ptr, rc::Rc, sync::Arc};

use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::{
    Foundation::{
        ERROR_BAD_COMMAND, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE,
        ERROR_MORE_DATA, ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, FACILITY_NTWIN32, INVALID_HANDLE_VALUE, NTSTATUS,
        RtlNtStatusToDosError, STATUS_SUCCESS,
    },
    Storage::FileSystem::SetFileCompletionNotificationModes,
    System::{
        IO::{
            CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY,
            PostQueuedCompletionStatus,
        },
        SystemServices::ERROR_SEVERITY_ERROR,
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

use super::{Overlapped, port::Port};
// use super::{TcpStream, UnixStream, stream::StreamOps};
use crate::channel::Receiver;

pub trait Handler {
    /// Operation is completed.
    fn completed(&mut self, id: u32);

    /// The driver's turn has completed.
    fn tick(&mut self);

    /// Clean up the handle before dropping the driver.
    fn cleanup(&mut self);
}

pub struct DriverApi {
    hnd: u32,
    reactor: Reactor,
}

impl DriverApi {
    /// Attach file descriptor.
    pub fn attach(&mut self, fd: RawHandle) -> io::Result<()> {
        self.reactor.attach(fd)
    }

    /// Get overlapped.
    pub fn overlapped(&mut self, id: u32) -> Overlapped {
        Overlapped::new(self.hnd, id)
    }

    #[inline]
    /// Attempt to cancel an already issued operation.
    pub fn cancel(&self, fd: RawHandle) {}
}

/// Low-level driver of io-uring.
pub struct Driver {
    hid: Cell<u32>,
    reactor: Reactor,
    #[allow(clippy::box_collection)]
    handlers: Cell<Option<Box<Vec<HandlerItem>>>>,
}

struct HandlerItem {
    hnd: Box<dyn Handler>,
    modified: bool,
}

impl HandlerItem {
    fn tick(&mut self) {
        if self.modified {
            self.modified = false;
            self.hnd.tick();
        }
    }
}

impl Driver {
    /// Create iocp driver
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            hid: Cell::new(0),
            reactor: Reactor::new(Port::new()?),
            handlers: Cell::new(Some(Box::new(Vec::new()))),
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
        handlers.push(HandlerItem {
            hnd: f(DriverApi {
                hnd,
                reactor: self.reactor.clone(),
            }),
            modified: false,
        });
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
        todo!()
    }

    fn unix_connect(&self, addr: std::path::PathBuf, cfg: SharedCfg) -> Receiver<Io> {
        todo!()
    }

    fn from_tcp_stream(&self, stream: net::TcpStream, cfg: SharedCfg) -> io::Result<Io> {
        todo!()
    }

    fn from_unix_stream(&self, stream: OsUnixStream, cfg: SharedCfg) -> io::Result<Io> {
        todo!()
    }
}

impl ntex_rt::Driver for Driver {
    /// Poll the driver and handle completed operations.
    fn run(&self, rt: &Runtime) -> io::Result<()> {
        todo!()
    }

    /// Get notification handle
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(self.reactor.handle())
    }
}

#[derive(Debug)]
struct Reactor(Arc<ReactorInner>);

#[derive(Debug)]
struct ReactorInner {
    port: OwnedHandle,
    overlapped: Overlapped,
    awake: AwakeFlag,
}

impl Reactor {
    fn new(port: Port) -> io::Result<Self> {
        let port = unsafe {
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, ptr::null_mut(), 0, 1);
            if port.is_null() {
                return Err(io::Error::last_os_error());
            }
            OwnedHandle::from_raw_handle(port)
        };
        log::trace!("New iocp handle: {port:?}");

        Self(Arc::new(ReactorInner {
            port,
            overlapped: Overlapped::new(0, 0),
            awake: AwakeFlag::new(),
        }))
    }

    fn attach(&self, fd: RawHandle) -> io::Result<()> {
        syscall!(
            BOOL,
            CreateIoCompletionPort(fd, self.port.as_raw_handle(), 0, 0) as isize
        )?;
        syscall!(
            BOOL,
            SetFileCompletionNotificationModes(
                fd,
                (FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE) as _
            )
        )?;
        Ok(())
    }

    fn set_awake(&self) {
        self.0.awake.set();
    }

    fn reset(&self) -> bool {
        self.0.awake.reset()
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

impl ReactorHandle {
    pub(crate) fn new(inner: Arc<ReactorInner>) -> Self {
        Self { inner }
    }
}

unsafe impl Send for ReactorHandle {}
unsafe impl Sync for ReactorHandle {}

impl Notify for ReactorHandle {
    /// Notify the driver.
    fn notify(&self) -> io::Result<()> {
        if !self.inner.awake.wake() {
            self.inner.port.post_raw(&self.inner.overlapped).ok();
        }
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
