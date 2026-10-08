//! Content-Encoding support
//!
//! Supports `gzip`, `deflate` and `zstd`. Large chunks are encoded and
//! decoded on the blocking thread pool, smaller ones on the current thread.
use std::io;

use crate::rt::{BlockingResult, spawn_blocking};
use crate::util::{Bytes, BytesMut};

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

struct Writer {
    buf: BytesMut,
}

impl Writer {
    fn new() -> Writer {
        Writer {
            buf: BytesMut::with_capacity(8192),
        }
    }

    fn take(&mut self) -> Bytes {
        self.buf.take()
    }

    fn len(&self) -> usize {
        self.buf.len()
    }
}

impl io::Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
