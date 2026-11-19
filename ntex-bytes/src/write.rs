use std::{fmt, io, ptr};

use crate::BytesMut;

/// Appends bytes to the spare capacity of a [`BytesMut`].
///
/// Created by [`BytesMut::spare_writer`]. Bytes are written into the spare
/// capacity, which is not initialized first, and the writer reserves more
/// capacity when it is full. The buffer length is updated when the writer
/// is dropped. If the writer is leaked, the buffer may not include all of
/// the written bytes.
pub struct SpareWriter<'a> {
    buf: &'a mut BytesMut,
    /// Buffer length when the writer was created.
    base: usize,
    /// Position of the first written byte, at `base` in the buffer.
    ptr: *mut u8,
    /// Number of bytes that fit at `ptr`.
    cap: usize,
    /// Number of written bytes.
    len: usize,
}

// SAFETY: The writer exclusively borrows the buffer, `ptr` points into its
// spare capacity.
unsafe impl Send for SpareWriter<'_> {}
// SAFETY: A shared reference only reads the written length.
unsafe impl Sync for SpareWriter<'_> {}

impl<'a> SpareWriter<'a> {
    #[inline]
    pub(crate) fn new(buf: &'a mut BytesMut, additional: usize) -> Self {
        buf.reserve(additional);
        let base = buf.len();
        let spare = buf.storage.spare_mut();
        let (ptr, cap) = (spare.as_mut_ptr(), spare.len());
        SpareWriter {
            buf,
            base,
            ptr,
            cap,
            len: 0,
        }
    }
}

impl SpareWriter<'_> {
    /// Appends a byte.
    #[inline]
    pub fn push(&mut self, byte: u8) {
        if self.len == self.cap {
            self.grow(1);
        }
        // SAFETY: `len < cap`, the byte is in the spare capacity of the buffer
        unsafe { self.ptr.add(self.len).write(byte) };
        self.len += 1;
    }

    /// Appends a byte slice.
    #[inline]
    pub fn extend_from_slice(&mut self, src: &[u8]) {
        if src.len() > self.cap - self.len {
            self.grow(src.len());
        }
        // SAFETY: `src` fits in the spare capacity after the written bytes
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.add(self.len), src.len()) };
        self.len += src.len();
    }

    /// Returns the number of written bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if no bytes have been written.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    fn grow(&mut self, additional: usize) {
        let (ptr, spare) = grow_spare(self.buf, self.base + self.len, additional);
        // SAFETY: The written bytes are the last `len` bytes of the buffer, right
        // before its spare capacity
        self.ptr = unsafe { ptr.sub(self.len) };
        self.cap = self.len + spare;
    }
}

/// Sets the length of `buf` to `len`, reserves `additional` bytes and returns
/// the spare capacity.
///
/// Takes the fields instead of the writer, so the writer can stay in registers.
#[cold]
#[inline(never)]
fn grow_spare(buf: &mut BytesMut, len: usize, additional: usize) -> (*mut u8, usize) {
    // SAFETY: The writer initialized the bytes up to `len`. Setting the length
    // again on drop, if `reserve` panics, sets the same value.
    unsafe { buf.set_len(len) };
    buf.reserve(additional);
    let spare = buf.storage.spare_mut();
    (spare.as_mut_ptr(), spare.len())
}

impl Drop for SpareWriter<'_> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: The writer initialized `len` bytes from `base`
        unsafe { self.buf.set_len(self.base + self.len) };
    }
}

impl io::Write for SpareWriter<'_> {
    #[inline]
    fn write(&mut self, src: &[u8]) -> Result<usize, io::Error> {
        self.extend_from_slice(src);
        Ok(src.len())
    }

    #[inline]
    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl fmt::Debug for SpareWriter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpareWriter")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: A `SpareWriter` keeps every written byte when it grows the buffer,
    /// including a buffer shared with split off `Bytes`.
    ///
    /// HOW:
    /// - Split a 4-byte head off a buffer, so the storage is shared, and start a writer with no reserve.
    /// - Write 10,000 bytes through `push`, `extend_from_slice` and `io::Write`.
    /// - Check the writer length, the buffer contents after the drop, and the untouched head.
    #[test]
    fn spare_writer_grows() {
        let mut buf = BytesMut::with_capacity(8);
        buf.extend_from_slice(b"headtail");
        let head = buf.split_to(4);

        let mut expected = b"tail".to_vec();
        let mut w = buf.spare_writer(0);
        for i in 0..2000_u32 {
            let b = (i % 251) as u8;
            w.push(b);
            w.extend_from_slice(&[b, b]);
            io::Write::write_all(&mut w, &[b, b]).unwrap();
            expected.extend_from_slice(&[b; 5]);
        }
        assert_eq!(w.len(), 10_000);
        drop(w);

        assert_eq!(&buf[..], &expected[..]);
        assert_eq!(&head[..], b"head");
    }

    /// STATEMENT: A `SpareWriter` adds nothing to the buffer when it writes nothing or
    /// is leaked before it grows.
    ///
    /// HOW:
    /// - Drop a writer with no writes and check the length and `is_empty`.
    /// - Leak a writer after pushing bytes into reserved capacity and check the length is unchanged.
    #[test]
    fn spare_writer_uncommitted() {
        let mut buf = BytesMut::copy_from_slice(b"abc");
        let w = buf.spare_writer(16);
        assert!(w.is_empty());
        drop(w);
        assert_eq!(&buf[..], b"abc");

        let mut w = buf.spare_writer(16);
        w.extend_from_slice(b"defg");
        std::mem::forget(w);
        assert_eq!(&buf[..], b"abc");
    }
}
