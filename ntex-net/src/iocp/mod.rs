use std::fmt;

use socket2::Socket;
use windows_sys::Win32::System::IO::OVERLAPPED;

//pub(crate) mod connect;
mod driver;
mod io;
mod stream;

pub use self::driver::{Driver, DriverApi, Handler};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, stream::StreamOps);

/// The overlapped struct for IOCP ops.
#[repr(C)]
pub struct Overlapped {
    /// The base [`OVERLAPPED`].
    pub(crate) base: OVERLAPPED,
    /// User data
    pub(crate) hnd: u32,
    pub(crate) udata: u32,
}

impl Overlapped {
    pub(crate) fn new(hnd: u32, udata: u32) -> Self {
        Self {
            hnd,
            udata,
            base: unsafe { std::mem::zeroed() },
        }
    }

    pub fn as_overlapped(&self) -> *const OVERLAPPED {
        &self.base
    }
}

impl fmt::Debug for Overlapped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Overlapped")
            .field("base", &"OVERLAPPED")
            .field("hnd", &self.hnd)
            .field("udata", &self.udata)
            .finish()
    }
}
