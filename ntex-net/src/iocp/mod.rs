use std::fmt;

use socket2::{SockAddr, Socket};
use windows_sys::Win32::System::IO::OVERLAPPED;

mod connect;
mod io;
mod ops;
mod reactor;
mod stream;

pub use self::reactor::{Handler, Reactor, ReactorApi};

/// Tcp stream wrapper for neon `TcpStream`
struct TcpStream(Socket, SockAddr, stream::StreamOps);

/// Tcp stream wrapper for neon `UnixStream`
struct UnixStream(Socket, SockAddr, stream::StreamOps);

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

    pub fn as_overlapped(&self) -> *mut OVERLAPPED {
        (&raw const self.base).cast_mut()
    }

    pub fn user_data(&self) -> u32 {
        self.udata
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
