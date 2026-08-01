#![allow(unused_imports)]
use std::{io, os::windows::io::RawSocket, ptr, task::Poll};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
        ERROR_MORE_DATA, ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_NOT_FOUND,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GetLastError,
    },
    Networking::WinSock::{SIO_GET_EXTENSION_FUNCTION_POINTER, WSABUF, WSARecv},
    System::IO::CancelIoEx,
};

use ntex_bytes::{BufMut, BytePage, BytesMut};
use ntex_io::{IoContext, IoTaskStatus};

use super::{DriverApi, Overlapped};

pub(crate) const RD_OP: u32 = 1;
pub(crate) const WR_OP: u32 = 2;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const WAIT      = 0b0000_0001;
        const CANCELING = 0b0000_0010;
        const REISSUE   = 0b0000_0100;
    }
}

#[repr(C)]
#[derive(Debug)]
pub(crate) struct ReadOperation {
    overlapped: Overlapped,
    id: usize, // idx for StreamItem
    io: RawSocket,
    ctx: IoContext,
    buf: Option<BytesMut>,
    flags: Flags,
}

impl ReadOperation {
    pub(crate) fn new(id: usize, io: RawSocket, ctx: IoContext, api: &DriverApi) -> Self {
        Self {
            overlapped: api.overlapped(RD_OP),
            id,
            io,
            ctx,
            buf: None,
            flags: Flags::empty(),
        }
    }

    pub(crate) fn tag(&self) -> &'static str {
        self.ctx.tag()
    }

    pub(crate) fn read(&mut self) {
        if !self.flags.contains(Flags::WAIT) {
            #[cfg(feature = "trace")]
            log::trace!("{}: Rcv({})", self.ctx.tag(), self.io);

            loop {
                let mut buf = self.ctx.get_read_buf();
                let s = buf.chunk_mut();
                let lpbufs = [WSABUF {
                    len: s.len() as u32,
                    buf: s.as_mut_ptr(),
                }];
                let mut size = 0;
                let mut flags = 0;
                let result = unsafe {
                    WSARecv(
                        self.io as _,
                        ptr::from_ref(&lpbufs[0]),
                        1,
                        &raw mut size,
                        &raw mut flags,
                        self.overlapped.as_overlapped(),
                        None,
                    )
                };

                let st = match winsock_result(result) {
                    Poll::Ready(Ok(())) => {
                        // SAFETY: windows tells us how many bytes it read
                        unsafe { buf.advance_mut(size as usize) };
                        self.ctx.update_read_status(buf, Ok(size as usize))
                    }
                    Poll::Ready(Err(err)) => self.ctx.update_read_status(buf, Err(err)),
                    Poll::Pending => {
                        self.buf = Some(buf);
                        self.flags.insert(Flags::WAIT);
                        return;
                    }
                };

                if st != IoTaskStatus::Io {
                    break;
                }
            }
        }
    }

    pub(crate) fn completed(res: io::Result<usize>, optr: *mut Overlapped) {
        let rd_optr: *mut ReadOperation = optr.cast();
        let rd = unsafe { &mut *rd_optr };

        #[cfg(feature = "trace")]
        log::trace!("{}: RcvDone({}) {res:?}", rd.ctx.tag(), rd.io);

        let mut buf = rd.buf.take().unwrap();
        rd.flags.remove(Flags::WAIT);

        let st = match res {
            Ok(size) => {
                // SAFETY: windows tells us how many bytes it read
                unsafe { buf.advance_mut(size) };
                rd.ctx.update_read_status(buf, Ok(size))
            }
            Err(err) => rd.ctx.update_read_status(buf, Err(err)),
        };
        if st == IoTaskStatus::Io {
            rd.read();
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub(crate) struct WriteOperation {
    overlapped: Overlapped,
    id: usize, // idx for StreamItem
    io: RawSocket,
    ctx: IoContext,
    pages: [Option<BytePage>; 16],
}

impl WriteOperation {
    pub(crate) fn new(id: usize, io: RawSocket, ctx: IoContext, api: &DriverApi) -> Self {
        Self {
            overlapped: api.overlapped(WR_OP),
            id,
            io,
            ctx,
            pages: [const { None }; 16],
        }
    }

    pub(crate) fn completed(res: io::Result<usize>, optr: *mut Overlapped) {
        let wr_optr: *mut WriteOperation = optr.cast();
        let wr = unsafe { &*wr_optr };

        println!("write completed === {res:?} == {wr:#?}");
    }
}

fn winapi_result() -> Poll<io::Result<()>> {
    let error = unsafe { GetLastError() };
    assert_ne!(error, 0);
    match error {
        ERROR_IO_PENDING => Poll::Pending,
        ERROR_IO_INCOMPLETE
        | ERROR_NETNAME_DELETED
        | ERROR_HANDLE_EOF
        | ERROR_BROKEN_PIPE
        | ERROR_PIPE_CONNECTED
        | ERROR_PIPE_NOT_CONNECTED
        | ERROR_NO_DATA
        | ERROR_MORE_DATA => Poll::Ready(Ok(())),
        _ => Poll::Ready(Err(io::Error::from_raw_os_error(error.cast_signed()))),
    }
}

fn win32_result(res: i32) -> Poll<io::Result<()>> {
    if res == 0 {
        winapi_result()
    } else {
        Poll::Ready(Ok(()))
    }
}

fn winsock_result(res: i32) -> Poll<io::Result<()>> {
    if res != 0 {
        winapi_result()
    } else {
        Poll::Ready(Ok(()))
    }
}
