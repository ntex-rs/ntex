//! Content-Encoding support
//!
//! Supports `gzip`, `deflate` and `zstd`. Large chunks are encoded and
//! decoded on the blocking thread pool, smaller ones on the current thread.
use zstd::zstd_safe::WriteBuf;

use crate::rt::{BlockingResult, spawn_blocking};
use crate::util::{BufMut, BytesMut};

mod decoder;
mod encoder;

pub use self::decoder::Decoder;
pub use self::encoder::Encoder;

#[cfg(test)]
thread_local! {
    static OFFLOADED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Run `f` on the blocking thread pool.
fn offload<F, R>(f: F) -> BlockingResult<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    #[cfg(test)]
    OFFLOADED.with(|n| n.set(n.get() + 1));
    spawn_blocking(f)
}

#[cfg(test)]
fn offloaded() -> usize {
    OFFLOADED.with(std::cell::Cell::get)
}

/// The spare capacity of a buffer, `zstd` writes its output directly into it.
struct Spare<'a> {
    buf: &'a mut BytesMut,
    start: usize,
    ptr: *mut u8,
    capacity: usize,
}

impl<'a> Spare<'a> {
    fn new(buf: &'a mut BytesMut) -> Self {
        let start = buf.len();
        let spare = buf.chunk_mut();
        let (ptr, capacity) = (spare.as_mut_ptr(), spare.len());
        Spare {
            buf,
            start,
            ptr,
            capacity,
        }
    }
}

// SAFETY: `ptr` points to `capacity` bytes of the buffer's spare capacity, and
// `filled_until` only extends the buffer over bytes zstd has written.
unsafe impl WriteBuf for Spare<'_> {
    fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    unsafe fn filled_until(&mut self, n: usize) {
        unsafe { self.buf.set_len(self.start + n) }
    }
}
