//! Utilities for encoding and decoding frames.
//!
//! Contains adapters to go from streams of bytes, [`AsyncRead`] and
//! [`AsyncWrite`], to framed streams implementing [`Sink`] and [`Stream`].
//! Framed streams are also known as [transports].
//!
//! [`AsyncRead`]: #
//! [`AsyncWrite`]: #
#![deny(rust_2018_idioms, warnings)]
use std::{io, mem::MaybeUninit, pin::Pin, task::Poll};

mod bcodec;
mod decoder;
mod encoder;
mod framed;

pub use self::bcodec::BytesCodec;
pub use self::decoder::Decoder;
pub use self::encoder::Encoder;
pub use self::framed::{Framed, FramedParts};

pub use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use ntex_bytes::{BufferSource, Owned};

pub async fn read_buf<T: AsyncRead, P: BufferSource>(
    mut io: Pin<&mut T>,
    buf: &mut P::Owned,
    pool: P,
    min_capacity: usize,
) -> io::Result<usize> {
    let remaining = buf.unfilled_mut().len();
    if remaining == 0 {
        let src = buf.filled();

        let num_bytes_to_copy_over = buf.filled().len();
        let new_len = std::cmp::max(min_capacity, num_bytes_to_copy_over * 2);
        let mut new_buf = pool.take(new_len).await;

        let dst = new_buf.unfilled_mut();
        assert!(dst.len() > num_bytes_to_copy_over);
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), src.len());
            new_buf.fill(num_bytes_to_copy_over);
        }

        *buf = new_buf;
    }

    let n = futures_util::future::poll_fn(|cx| {
        let dst = unsafe { &mut *(buf.unfilled_mut() as *mut _ as *mut [MaybeUninit<u8>]) };
        let mut buf = ReadBuf::uninit(dst);

        let ptr = buf.filled().as_ptr();

        if io.as_mut().poll_read(cx, &mut buf)?.is_pending() {
            return Poll::Pending;
        }

        // Ensure the pointer does not change from under us
        assert_eq!(ptr, buf.filled().as_ptr());

        Poll::Ready(Ok::<_, io::Error>(buf.filled().len()))
    }).await?;

    // Safety: This is guaranteed to be the number of initialized (and read)
    // bytes due to the invariants provided by `ReadBuf::filled`.
    unsafe {
        buf.fill(n);
    }

    Ok(n)
}
