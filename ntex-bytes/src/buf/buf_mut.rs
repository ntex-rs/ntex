use std::{cmp, mem, ptr};

use super::{UninitSlice, Writer};

/// A trait for values that provide sequential write access to bytes.
///
/// Write bytes to a buffer
///
/// A buffer stores bytes in memory such that write operations are infallible.
/// The underlying storage may or may not be in contiguous memory. A `BufMut`
/// value is a cursor into the buffer. Writing to `BufMut` advances the cursor
/// position.
///
/// The simplest `BufMut` is a `Vec<u8>`.
///
/// ```
/// use ntex_bytes::BufMut;
///
/// let mut buf = vec![];
///
/// buf.put("hello world");
///
/// assert_eq!(buf, b"hello world");
/// ```
pub trait BufMut {
    /// Returns the number of bytes that can be written from the current
    /// position until the end of the buffer is reached.
    ///
    /// This value is greater than or equal to the length of the slice returned
    /// by `chunk_mut`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut dst = [0; 10];
    /// let mut buf = &mut dst[..];
    ///
    /// let original_remaining = buf.remaining_mut();
    /// buf.put("hello");
    ///
    /// assert_eq!(original_remaining - 5, buf.remaining_mut());
    /// ```
    ///
    /// # Implementer notes
    ///
    /// Implementations of `remaining_mut` should ensure that the return value
    /// does not change unless a call is made to `advance_mut` or any other
    /// function that is documented to change the `BufMut`'s current position.
    fn remaining_mut(&self) -> usize;

    /// Advance the internal cursor of the `BufMut`
    ///
    /// The next call to `bytes_mut` will return a slice starting `cnt` bytes
    /// further into the underlying buffer.
    ///
    /// This function is unsafe because there is no guarantee that the bytes
    /// being advanced past have been initialized.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = Vec::with_capacity(16);
    ///
    /// unsafe {
    ///     buf.chunk_mut()[0..2].copy_from_slice(b"he");
    ///     buf.advance_mut(2);
    ///
    ///     buf.chunk_mut()[0..3].copy_from_slice(b"llo");
    ///     buf.advance_mut(3);
    /// }
    ///
    /// assert_eq!(5, buf.len());
    /// assert_eq!(buf, b"hello");
    /// ```
    ///
    /// # Panics
    ///
    /// This function **may** panic if `cnt > self.remaining_mut()`.
    ///
    /// # Implementer notes
    ///
    /// It is recommended for implementations of `advance_mut` to panic if
    /// `cnt > self.remaining_mut()`. If the implementation does not panic,
    /// the call must behave as if `cnt == self.remaining_mut()`.
    ///
    /// A call with `cnt == 0` should never panic and be a no-op.
    #[allow(clippy::missing_safety_doc)]
    unsafe fn advance_mut(&mut self, cnt: usize);

    /// Returns true if there is space in `self` for more bytes.
    ///
    /// This is equivalent to `self.remaining_mut() != 0`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut dst = [0; 5];
    /// let mut buf = &mut dst[..];
    ///
    /// assert!(buf.has_remaining_mut());
    ///
    /// buf.put("hello");
    ///
    /// assert!(!buf.has_remaining_mut());
    /// ```
    #[inline]
    fn has_remaining_mut(&self) -> bool {
        self.remaining_mut() > 0
    }

    /// Returns a mutable slice starting at the current `BufMut` position and of
    /// length between 0 and `BufMut::remaining_mut()`. Note that this *can* be shorter than the
    /// whole remainder of the buffer (this allows non-continuous implementation).
    ///
    /// This is a lower level function. Most operations are done with other
    /// functions.
    ///
    /// The returned byte slice may represent uninitialized memory.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = Vec::with_capacity(16);
    ///
    /// unsafe {
    ///     buf.chunk_mut()[0..2].copy_from_slice(b"he");
    ///
    ///     buf.advance_mut(2);
    ///
    ///     buf.chunk_mut()[0..3].copy_from_slice(b"llo");
    ///
    ///     buf.advance_mut(3);
    /// }
    ///
    /// assert_eq!(5, buf.len());
    /// assert_eq!(buf, b"hello");
    /// ```
    ///
    /// # Implementer notes
    ///
    /// This function should never panic. `bytes_mut` should return an empty
    /// slice **if and only if** `remaining_mut` returns 0. In other words,
    /// `bytes_mut` returning an empty slice implies that `remaining_mut` will
    /// return 0 and `remaining_mut` returning 0 implies that `bytes_mut` will
    /// return an empty slice.
    fn chunk_mut(&mut self) -> &mut UninitSlice;

    /// Transfer bytes into `self` from `src` and advance the cursor by the
    /// number of bytes written.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    ///
    /// buf.put_u8(b'h');
    /// buf.put(&b"ello"[..]);
    /// buf.put(" world");
    ///
    /// assert_eq!(buf, b"hello world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `self` does not have enough capacity to contain `src`.
    fn put<T: super::Buf>(&mut self, mut src: T)
    where
        Self: Sized,
    {
        assert!(self.remaining_mut() >= src.remaining());

        while src.has_remaining() {
            let s = src.chunk();
            let d = self.chunk_mut();
            let l = cmp::min(s.len(), d.len());

            unsafe {
                ptr::copy_nonoverlapping(s.as_ptr(), d.as_mut_ptr(), l);
            }

            src.advance(l);
            unsafe {
                self.advance_mut(l);
            }
        }
    }

    /// Transfer bytes into `self` from `src` and advance the cursor by the
    /// number of bytes written.
    ///
    /// `self` must have enough remaining capacity to contain all of `src`.
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut dst = [0; 6];
    ///
    /// {
    ///     let mut buf = &mut dst[..];
    ///     buf.put_slice(b"hello");
    ///
    ///     assert_eq!(1, buf.remaining_mut());
    /// }
    ///
    /// assert_eq!(b"hello\0", &dst);
    /// ```
    fn put_slice(&mut self, src: &[u8]) {
        let mut off = 0;

        assert!(self.remaining_mut() >= src.len(), "buffer overflow");

        while off < src.len() {
            let cnt;

            unsafe {
                let dst = self.chunk_mut();
                cnt = cmp::min(dst.len(), src.len() - off);

                ptr::copy_nonoverlapping(src[off..].as_ptr(), dst.as_mut_ptr(), cnt);

                off += cnt;
            }

            unsafe {
                self.advance_mut(cnt);
            }
        }
    }

    /// Writes an unsigned 8 bit integer to `self`.
    ///
    /// The current position is advanced by 1.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u8(0x01);
    /// assert_eq!(buf, b"\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u8(&mut self, n: u8) {
        let src = [n];
        self.put_slice(&src);
    }

    /// Writes a signed 8 bit integer to `self`.
    ///
    /// The current position is advanced by 1.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i8(0x01);
    /// assert_eq!(buf, b"\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    fn put_i8(&mut self, n: i8) {
        self.put_slice(&[n as u8]);
    }

    /// Writes an unsigned 16 bit integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by 2.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u16(0x0809);
    /// assert_eq!(buf, b"\x08\x09");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u16(&mut self, n: u16) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes an unsigned 16 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 2.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u16_le(0x0809);
    /// assert_eq!(buf, b"\x09\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u16_le(&mut self, n: u16) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes a signed 16 bit integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by 2.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i16(0x0809);
    /// assert_eq!(buf, b"\x08\x09");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i16(&mut self, n: i16) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes a signed 16 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 2.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i16_le(0x0809);
    /// assert_eq!(buf, b"\x09\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i16_le(&mut self, n: i16) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes an unsigned 32 bit integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u32(0x0809A0A1);
    /// assert_eq!(buf, b"\x08\x09\xA0\xA1");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u32(&mut self, n: u32) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes an unsigned 32 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u32_le(0x0809A0A1);
    /// assert_eq!(buf, b"\xA1\xA0\x09\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u32_le(&mut self, n: u32) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes a signed 32 bit integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i32(0x0809A0A1);
    /// assert_eq!(buf, b"\x08\x09\xA0\xA1");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i32(&mut self, n: i32) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes a signed 32 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i32_le(0x0809A0A1);
    /// assert_eq!(buf, b"\xA1\xA0\x09\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i32_le(&mut self, n: i32) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes an unsigned 64 bit integer to `self` in the big-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u64(0x0102030405060708);
    /// assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u64(&mut self, n: u64) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes an unsigned 64 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u64_le(0x0102030405060708);
    /// assert_eq!(buf, b"\x08\x07\x06\x05\x04\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u64_le(&mut self, n: u64) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes a signed 64 bit integer to `self` in the big-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i64(0x0102030405060708);
    /// assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i64(&mut self, n: i64) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes a signed 64 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i64_le(0x0102030405060708);
    /// assert_eq!(buf, b"\x08\x07\x06\x05\x04\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i64_le(&mut self, n: i64) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes an unsigned 128 bit integer to `self` in the big-endian byte order.
    ///
    /// The current position is advanced by 16.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u128(0x01020304050607080910111213141516);
    /// assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x10\x11\x12\x13\x14\x15\x16");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u128(&mut self, n: u128) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes an unsigned 128 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 16.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_u128_le(0x01020304050607080910111213141516);
    /// assert_eq!(buf, b"\x16\x15\x14\x13\x12\x11\x10\x09\x08\x07\x06\x05\x04\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_u128_le(&mut self, n: u128) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes a signed 128 bit integer to `self` in the big-endian byte order.
    ///
    /// The current position is advanced by 16.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i128(0x01020304050607080910111213141516);
    /// assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x10\x11\x12\x13\x14\x15\x16");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i128(&mut self, n: i128) {
        self.put_slice(&n.to_be_bytes());
    }

    /// Writes a signed 128 bit integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by 16.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_i128_le(0x01020304050607080910111213141516);
    /// assert_eq!(buf, b"\x16\x15\x14\x13\x12\x11\x10\x09\x08\x07\x06\x05\x04\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_i128_le(&mut self, n: i128) {
        self.put_slice(&n.to_le_bytes());
    }

    /// Writes an unsigned n-byte integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by `nbytes`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_uint(0x010203, 3);
    /// assert_eq!(buf, b"\x01\x02\x03");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_uint(&mut self, n: u64, nbytes: usize) {
        self.put_slice(&n.to_be_bytes()[mem::size_of_val(&n) - nbytes..]);
    }

    /// Writes an unsigned n-byte integer to `self` in the little-endian byte order.
    ///
    /// The current position is advanced by `nbytes`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_uint_le(0x010203, 3);
    /// assert_eq!(buf, b"\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_uint_le(&mut self, n: u64, nbytes: usize) {
        self.put_slice(&n.to_le_bytes()[0..nbytes]);
    }

    /// Writes a signed n-byte integer to `self` in big-endian byte order.
    ///
    /// The current position is advanced by `nbytes`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_int(0x010203, 3);
    /// assert_eq!(buf, b"\x01\x02\x03");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_int(&mut self, n: i64, nbytes: usize) {
        self.put_slice(&n.to_be_bytes()[mem::size_of_val(&n) - nbytes..]);
    }

    /// Writes a signed n-byte integer to `self` in little-endian byte order.
    ///
    /// The current position is advanced by `nbytes`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_int_le(0x010203, 3);
    /// assert_eq!(buf, b"\x03\x02\x01");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_int_le(&mut self, n: i64, nbytes: usize) {
        self.put_slice(&n.to_le_bytes()[0..nbytes]);
    }

    /// Writes  an IEEE754 single-precision (4 bytes) floating point number to
    /// `self` in big-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_f32(1.2f32);
    /// assert_eq!(buf, b"\x3F\x99\x99\x9A");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_f32(&mut self, n: f32) {
        self.put_u32(n.to_bits());
    }

    /// Writes  an IEEE754 single-precision (4 bytes) floating point number to
    /// `self` in little-endian byte order.
    ///
    /// The current position is advanced by 4.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_f32_le(1.2f32);
    /// assert_eq!(buf, b"\x9A\x99\x99\x3F");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_f32_le(&mut self, n: f32) {
        self.put_u32_le(n.to_bits());
    }

    /// Writes  an IEEE754 double-precision (8 bytes) floating point number to
    /// `self` in big-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_f64(1.2f64);
    /// assert_eq!(buf, b"\x3F\xF3\x33\x33\x33\x33\x33\x33");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_f64(&mut self, n: f64) {
        self.put_u64(n.to_bits());
    }

    /// Writes  an IEEE754 double-precision (8 bytes) floating point number to
    /// `self` in little-endian byte order.
    ///
    /// The current position is advanced by 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut buf = vec![];
    /// buf.put_f64_le(1.2f64);
    /// assert_eq!(buf, b"\x33\x33\x33\x33\x33\x33\xF3\x3F");
    /// ```
    ///
    /// # Panics
    ///
    /// This function panics if there is not enough remaining capacity in
    /// `self`.
    #[inline]
    fn put_f64_le(&mut self, n: f64) {
        self.put_u64_le(n.to_bits());
    }

    /// Creates an adaptor which implements the `Write` trait for `self`.
    ///
    /// This function returns a new value which implements `Write` by adapting
    /// the `Write` trait functions to the `BufMut` trait functions. Given that
    /// `BufMut` operations are infallible, none of the `Write` functions will
    /// return with `Err`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    /// use std::io::Write;
    ///
    /// let mut buf = vec![].writer();
    ///
    /// let num = buf.write(&b"hello world"[..]).unwrap();
    /// assert_eq!(11, num);
    ///
    /// let buf = buf.into_inner();
    ///
    /// assert_eq!(*buf, b"hello world"[..]);
    /// ```
    fn writer(self) -> Writer<Self>
    where
        Self: Sized,
    {
        Writer::new(self)
    }
}

impl<T: BufMut + ?Sized> BufMut for &mut T {
    fn remaining_mut(&self) -> usize {
        (**self).remaining_mut()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        (**self).chunk_mut()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        (**self).advance_mut(cnt);
    }
}

impl<T: BufMut + ?Sized> BufMut for Box<T> {
    fn remaining_mut(&self) -> usize {
        (**self).remaining_mut()
    }

    fn chunk_mut(&mut self) -> &mut UninitSlice {
        (**self).chunk_mut()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        (**self).advance_mut(cnt);
    }
}

impl BufMut for Vec<u8> {
    #[inline]
    fn remaining_mut(&self) -> usize {
        usize::MAX - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        let len = self.len();
        let remaining = self.capacity() - len;
        if cnt > remaining {
            // Reserve additional capacity, and ensure that the total length
            // will not overflow usize.
            self.reserve(cnt);
        }

        self.set_len(len + cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        if self.capacity() == self.len() {
            self.reserve(64); // Grow the vec
        }

        let cap = self.capacity();
        let len = self.len();

        let ptr = self.as_mut_ptr();
        unsafe { &mut UninitSlice::from_raw_parts_mut(ptr, cap)[len..] }
    }
}

impl BufMut for &mut [u8] {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        // UninitSlice is repr(transparent), so safe to transmute
        unsafe { &mut *(ptr::from_mut::<[u8]>(*self) as *mut _) }
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        // Lifetime dance taken from `impl Write for &mut [u8]`.
        let (_, b) = core::mem::take(self).split_at_mut(cnt);
        *self = b;
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        self[..src.len()].copy_from_slice(src);
        unsafe {
            self.advance_mut(src.len());
        }
    }
}

// The existence of this function makes the compiler catch if the BufMut
// trait is "object-safe" or not.
fn _assert_trait_object(_b: &dyn BufMut) {}

#[cfg(test)]
#[allow(unused_allocation, warnings)]
mod tests {
    use super::*;
    use crate::BytesMut;

    #[test]
    #[allow(clippy::needless_borrow)]
    fn buf_mut_tests() {
        let mut buf = vec![];
        buf.put_u8(0x01);
        assert_eq!(buf, b"\x01");

        assert_eq!(buf.remaining_mut(), usize::MAX - 1);
        assert_eq!((&buf).remaining_mut(), usize::MAX - 1);
        assert_eq!(Box::new(buf).remaining_mut(), usize::MAX - 1);

        let mut buf = [b'1'; 10];
        let mut b = buf.as_mut();
        assert_eq!(b.remaining_mut(), 10);
        assert!(b.has_remaining_mut());
        b.put_slice(b"123");
        assert_eq!(&buf[..], b"1231111111");

        let mut b: &mut [u8] = buf.as_mut();
        let chunk = b.chunk_mut();
        chunk.write_byte(0, b'9');
        assert_eq!(&buf[..], b"9231111111");

        let mut b = buf.as_mut();
        let chunk = b.chunk_mut();
        chunk.copy_from_slice(b"0000000000");
        assert_eq!(&buf[..], b"0000000000");

        let mut b = buf.as_mut();
        assert_eq!(format!("{:?}", b.chunk_mut()), "UninitSlice[...]");

        let b = buf.as_mut();
        let mut bb = Box::new(b);
        unsafe { bb.advance_mut(1) };
        let chunk = bb.chunk_mut();
        chunk.copy_from_slice(b"111111111");
        assert_eq!(&buf[..], b"0111111111");

        let mut buf = BytesMut::new();
        buf.put_u8(0x01);
        assert_eq!(buf, b"\x01"[..]);

        let mut buf = BytesMut::new();
        buf.put_u8(0x01);
        assert_eq!(buf, b"\x01"[..]);

        let mut buf = vec![];
        buf.put_i8(0x01);
        assert_eq!(buf, b"\x01");

        let mut buf = BytesMut::new();
        buf.put_i8(0x01);
        assert_eq!(buf, b"\x01"[..]);

        let mut buf = BytesMut::new();
        buf.put_i8(0x01);
        assert_eq!(buf, b"\x01"[..]);
        assert_eq!((&mut buf).remaining_mut(), 107);
        let chunk = (&mut buf).chunk_mut();
        chunk.write_byte(0, b'9');
        unsafe { (&mut buf).advance_mut(1) };
        assert_eq!((&mut buf).remaining_mut(), 106);

        let mut buf = vec![];
        buf.put_i16(0x0809);
        assert_eq!(buf, b"\x08\x09");

        let mut buf = vec![];
        buf.put_i16_le(0x0809);
        assert_eq!(buf, b"\x09\x08");

        let mut buf = vec![];
        buf.put_u32(0x0809A0A1);
        assert_eq!(buf, b"\x08\x09\xA0\xA1");

        let mut buf = vec![];
        buf.put_i32(0x0809A0A1);
        assert_eq!(buf, b"\x08\x09\xA0\xA1");

        let mut buf = vec![];
        buf.put_i32_le(0x0809A0A1);
        assert_eq!(buf, b"\xA1\xA0\x09\x08");

        let mut buf = vec![];
        buf.put_u64(0x0102030405060708);
        assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08");

        let mut buf = vec![];
        buf.put_u64_le(0x0102030405060708);
        assert_eq!(buf, b"\x08\x07\x06\x05\x04\x03\x02\x01");

        let mut buf = vec![];
        buf.put_i64(0x0102030405060708);
        assert_eq!(buf, b"\x01\x02\x03\x04\x05\x06\x07\x08");

        let mut buf = vec![];
        buf.put_i64_le(0x0102030405060708);
        assert_eq!(buf, b"\x08\x07\x06\x05\x04\x03\x02\x01");

        let mut buf = vec![];
        buf.put_u128(0x01020304050607080910111213141516);
        assert_eq!(
            buf,
            b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x10\x11\x12\x13\x14\x15\x16"
        );

        let mut buf = vec![];
        buf.put_u128_le(0x01020304050607080910111213141516);
        assert_eq!(
            buf,
            b"\x16\x15\x14\x13\x12\x11\x10\x09\x08\x07\x06\x05\x04\x03\x02\x01"
        );

        let mut buf = vec![];
        buf.put_i128(0x01020304050607080910111213141516);
        assert_eq!(
            buf,
            b"\x01\x02\x03\x04\x05\x06\x07\x08\x09\x10\x11\x12\x13\x14\x15\x16"
        );

        let mut buf = vec![];
        buf.put_i128_le(0x01020304050607080910111213141516);
        assert_eq!(
            buf,
            b"\x16\x15\x14\x13\x12\x11\x10\x09\x08\x07\x06\x05\x04\x03\x02\x01"
        );

        let mut buf = vec![];
        buf.put_uint(0x010203, 3);
        assert_eq!(buf, b"\x01\x02\x03");

        let mut buf = vec![];
        buf.put_uint_le(0x010203, 3);
        assert_eq!(buf, b"\x03\x02\x01");

        let mut buf = vec![];
        buf.put_int(0x010203, 3);
        assert_eq!(buf, b"\x01\x02\x03");

        let mut buf = vec![];
        buf.put_int_le(0x010203, 3);
        assert_eq!(buf, b"\x03\x02\x01");

        let mut buf = vec![];
        buf.put_f32(1.2f32);
        assert_eq!(buf, b"\x3F\x99\x99\x9A");

        let mut buf = vec![];
        buf.put_f32_le(1.2f32);
        assert_eq!(buf, b"\x9A\x99\x99\x3F");

        let mut buf = vec![];
        buf.put_f64(1.2f64);
        assert_eq!(buf, b"\x3F\xF3\x33\x33\x33\x33\x33\x33");

        let mut buf = vec![];
        buf.put_f64_le(1.2f64);
        assert_eq!(buf, b"\x33\x33\x33\x33\x33\x33\xF3\x3F");
    }
}
