use std::{any, io};

use compio_buf::{BufResult, IoBuf, IoBufMut, SetBufInit};
use compio_io::{AsyncRead, AsyncWrite};
use compio_net::TcpStream;
use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_io::{types, Handle, IoStream, ReadContext, WriteContext, WriteContextBuf};

struct CompioBuf(BytesVec);

unsafe impl IoBuf for CompioBuf {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.0.chunk().as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.0.remaining_mut()
    }
}

unsafe impl IoBufMut for CompioBuf {
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.0.chunk_mut().as_mut_ptr()
    }
}

impl SetBufInit for CompioBuf {
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.0.set_len(len + self.0.len());
    }
}
