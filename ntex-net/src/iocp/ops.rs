#![allow(clippy::cast_possible_wrap)]
use std::{cmp, io, mem, os::windows::io::RawSocket, ptr, task::Poll};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
        ERROR_MORE_DATA, ERROR_NETNAME_DELETED, ERROR_NO_DATA, ERROR_NOT_FOUND,
        ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED,
        GetLastError,
    },
    Networking::WinSock::{WSABUF, WSARecv, WSASend},
    System::IO::CancelIoEx,
};

use ntex_bytes::{BufMut, BytePage, BytesMut};
use ntex_io::{IoContext, IoTaskStatus};
use ntex_rt::syscall;

use super::{DriverApi, Overlapped};

pub(crate) const RD_OP: u32 = 1;
pub(crate) const WR_OP: u32 = 2;
const MAX_WRITE_BUFS: usize = 16;

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug)]
    struct Flags: u8 {
        const WAITING     = 0b0000_0001;
        const CLOSING     = 0b0000_0010;
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

    pub(crate) fn pause(&mut self, closing: bool) -> bool {
        if self.flags.contains(Flags::WAITING) {
            if let Err(err) = syscall!(
                BOOL,
                CancelIoEx(self.io as _, self.overlapped.as_overlapped())
            ) {
                let e = err.raw_os_error();
                if e != Some(ERROR_NOT_FOUND as _)
                    && e != Some(ERROR_OPERATION_ABORTED as _)
                {
                    self.ctx
                        .update_read_status(self.buf.take().unwrap(), Err(err));
                    return true;
                }
            }
            if closing {
                self.flags.insert(Flags::CLOSING);
            }
            false
        } else {
            true
        }
    }

    pub(crate) fn read(&mut self) {
        if !self.flags.contains(Flags::WAITING) {
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
                        self.flags.insert(Flags::WAITING);
                        return;
                    }
                };

                if st != IoTaskStatus::Io {
                    break;
                }
            }
        }
    }

    pub(crate) fn completed(
        res: io::Result<usize>,
        optr: *mut Overlapped,
    ) -> Option<usize> {
        let rd_optr: *mut ReadOperation = optr.cast();
        let rd = unsafe { &mut *rd_optr };
        rd.flags.remove(Flags::WAITING);

        #[cfg(feature = "trace")]
        log::trace!("{}: RcvDone({}) {res:?}", rd.ctx.tag(), rd.io);

        if let Some(mut buf) = rd.buf.take() {
            let st = match res {
                Ok(size) => {
                    // SAFETY: windows tells us how many bytes it read
                    unsafe { buf.advance_mut(size) };
                    rd.ctx.update_read_status(buf, Ok(size))
                }
                Err(err) => rd.ctx.update_read_status(buf, Err(err)),
            };
            if rd.flags.contains(Flags::CLOSING) {
                Some(rd.id)
            } else {
                if st == IoTaskStatus::Io {
                    rd.read();
                }
                None
            }
        } else if rd.flags.contains(Flags::CLOSING) {
            Some(rd.id)
        } else {
            None
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
    flags: Flags,
    pages: [Option<BytePage>; MAX_WRITE_BUFS],
}

impl WriteOperation {
    pub(crate) fn new(id: usize, io: RawSocket, ctx: IoContext, api: &DriverApi) -> Self {
        Self {
            overlapped: api.overlapped(WR_OP),
            id,
            io,
            ctx,
            flags: Flags::empty(),
            pages: [const { None }; MAX_WRITE_BUFS],
        }
    }

    pub(crate) fn pause(&mut self) -> bool {
        if self.flags.contains(Flags::WAITING) {
            if let Err(err) = syscall!(
                BOOL,
                CancelIoEx(self.io as _, self.overlapped.as_overlapped())
            ) {
                let e = err.raw_os_error();
                if e != Some(ERROR_NOT_FOUND as _)
                    && e != Some(ERROR_OPERATION_ABORTED as _)
                {
                    return true;
                }
            }
            self.flags.insert(Flags::CLOSING);
            false
        } else {
            true
        }
    }

    pub(crate) fn write(&mut self) {
        loop {
            if self.flags.contains(Flags::WAITING) {
                return;
            }
            let st = self.ctx.with_write_buf(|wrt| {
                #[cfg(feature = "trace")]
                log::trace!("{}: Wrt({}) size:{:?}", self.ctx.tag(), self.io, wrt.len());

                let mut lpbufs = [mem::MaybeUninit::<WSABUF>::uninit(); 16];
                let mut num = 0;
                while let Some(page) = wrt.take() {
                    self.pages[num] = Some(page);
                    let p = self.pages[num].as_ref().unwrap();

                    // SAFETY: Page is stored in `pages` for lifetime of `bufs`
                    lpbufs[num].write(WSABUF {
                        len: p.len() as u32,
                        buf: unsafe { p.as_ptr().cast_mut() },
                    });

                    num += 1;
                    if num == MAX_WRITE_BUFS {
                        break;
                    }
                }

                if num > 0 {
                    let mut sent = 0;
                    let result = unsafe {
                        WSASend(
                            self.io as _,
                            ptr::from_ref(&lpbufs[0]).cast(),
                            num as u32,
                            &raw mut sent,
                            0,
                            self.overlapped.as_overlapped(),
                            None,
                        )
                    };

                    match winsock_result(result) {
                        Poll::Ready(Ok(())) => {
                            let mut sent = sent as usize;
                            // remove written bytes
                            for page in self.pages[..num].iter_mut().flatten() {
                                let len = cmp::min(page.len(), sent);
                                page.advance_to(len);
                                sent -= len;
                                if sent == 0 {
                                    break;
                                }
                            }
                            // return unwritten data back to buffer
                            for p in self.pages[..num].iter_mut().rev() {
                                if let Some(page) = p.take() {
                                    wrt.prepend(page);
                                }
                            }
                            Ok(true)
                        }
                        Poll::Ready(Err(err)) => {
                            // return unwritten data back to buffer
                            for p in self.pages[..num].iter_mut().rev() {
                                if let Some(page) = p.take() {
                                    wrt.prepend(page);
                                }
                            }
                            Err(err)
                        }
                        Poll::Pending => {
                            self.flags.insert(Flags::WAITING);
                            Ok(false)
                        }
                    }
                } else {
                    Ok(false)
                }
            });

            if self.ctx.update_write_status(st) != IoTaskStatus::Io {
                break;
            }
        }
    }

    pub(crate) fn completed(
        res: io::Result<usize>,
        optr: *mut Overlapped,
    ) -> Option<usize> {
        let wr_optr: *mut WriteOperation = optr.cast();
        let wr = unsafe { &mut *wr_optr };

        #[cfg(feature = "trace")]
        log::trace!("{}: WrtDone({}) {res:?}", wr.ctx.tag(), wr.io);

        wr.flags.remove(Flags::WAITING);

        let st = match res {
            Ok(mut sent) => {
                // remove written bytes
                for page in wr.pages[..].iter_mut().flatten() {
                    let len = cmp::min(page.len(), sent);
                    page.advance_to(len);
                    sent -= len;
                    if sent == 0 {
                        break;
                    }
                }
                Ok(true)
            }
            Err(err) => Err(err),
        };

        // return unwritten data back to buffer
        wr.ctx.with_write_buf(|wrt| {
            for p in wr.pages[..].iter_mut().rev() {
                if let Some(page) = p.take() {
                    wrt.prepend(page);
                }
            }
        });

        if wr.flags.contains(Flags::CLOSING) {
            Some(wr.id)
        } else {
            if wr.ctx.update_write_status(st) == IoTaskStatus::Io {
                wr.write();
            }
            None
        }
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

pub(crate) fn win32_result(res: i32) -> Poll<io::Result<()>> {
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
