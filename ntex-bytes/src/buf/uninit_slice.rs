use std::ops::{
    Index, IndexMut, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo, RangeToInclusive,
};
use std::{fmt, mem::MaybeUninit, ptr};

/// Uninitialized byte slice.
///
/// Returned by `BufMut::chunk_mut()`, the referenced byte slice may be
/// uninitialized. The wrapper provides safe access without introducing
/// undefined behavior.
///
/// The safety invariants of this wrapper are:
///
///  1. Reading from an `UninitSlice` is undefined behavior.
///  2. Writing uninitialized bytes to an `UninitSlice` is undefined behavior.
///
/// The difference between `&mut UninitSlice` and `&mut [MaybeUninit<u8>]` is
/// that it is possible in safe code to write uninitialized bytes to an
/// `&mut [MaybeUninit<u8>]`, which this type prohibits.
#[repr(transparent)]
pub struct UninitSlice([MaybeUninit<u8>]);

impl UninitSlice {
    /// Create a `&mut UninitSlice` from a pointer and a length.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` references a valid memory region owned
    /// by the caller representing a byte slice for the duration of `'a`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::buf::UninitSlice;
    ///
    /// let bytes = b"hello world".to_vec();
    /// let ptr = bytes.as_ptr() as *mut _;
    /// let len = bytes.len();
    ///
    /// let slice = unsafe { UninitSlice::from_raw_parts_mut(ptr, len) };
    /// ```
    #[inline]
    pub unsafe fn from_raw_parts_mut<'a>(ptr: *mut u8, len: usize) -> &'a mut UninitSlice {
        let maybe_init: &mut [MaybeUninit<u8>] =
            core::slice::from_raw_parts_mut(ptr.cast(), len);
        &mut *(ptr::from_mut::<[MaybeUninit<u8>]>(maybe_init) as *mut UninitSlice)
    }

    /// Write a single byte at the specified offset.
    ///
    /// # Panics
    ///
    /// The function panics if `index` is out of bounds.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::buf::UninitSlice;
    ///
    /// let mut data = [b'f', b'o', b'o'];
    /// let slice = unsafe { UninitSlice::from_raw_parts_mut(data.as_mut_ptr(), 3) };
    ///
    /// slice.write_byte(0, b'b');
    ///
    /// assert_eq!(b"boo", &data[..]);
    /// ```
    #[inline]
    pub fn write_byte(&mut self, index: usize, byte: u8) {
        assert!(index < self.len());

        unsafe { self[index..].as_mut_ptr().write(byte) }
    }

    /// Copies bytes  from `src` into `self`.
    ///
    /// The length of `src` must be the same as `self`.
    ///
    /// # Panics
    ///
    /// The function panics if `src` has a different length than `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::buf::UninitSlice;
    ///
    /// let mut data = [b'f', b'o', b'o'];
    /// let slice = unsafe { UninitSlice::from_raw_parts_mut(data.as_mut_ptr(), 3) };
    ///
    /// slice.copy_from_slice(b"bar");
    ///
    /// assert_eq!(b"bar", &data[..]);
    /// ```
    #[inline]
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        use core::ptr;

        assert_eq!(self.len(), src.len());

        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.as_mut_ptr(), self.len());
        }
    }

    /// Return a raw pointer to the slice's buffer.
    ///
    /// // Safety
    ///
    /// The caller **must not** read from the referenced memory and **must not**
    /// write **uninitialized** bytes to the slice either.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut data = [0, 1, 2];
    /// let mut slice = &mut data[..];
    /// let ptr = BufMut::chunk_mut(&mut slice).as_mut_ptr();
    /// ```
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr().cast()
    }

    /// Returns the number of bytes in the slice.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BufMut;
    ///
    /// let mut data = [0, 1, 2];
    /// let mut slice = &mut data[..];
    /// let len = BufMut::chunk_mut(&mut slice).len();
    ///
    /// assert_eq!(len, 3);
    /// ```
    #[inline]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl AsMut<[MaybeUninit<u8>]> for UninitSlice {
    fn as_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.0
    }
}

impl fmt::Debug for UninitSlice {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("UninitSlice[...]").finish()
    }
}

macro_rules! impl_index {
    ($($t:ty),*) => {
        $(
            impl Index<$t> for UninitSlice {
                type Output = UninitSlice;

                #[inline]
                fn index(&self, index: $t) -> &UninitSlice {
                    let maybe_uninit: &[MaybeUninit<u8>] = &self.0[index];
                    unsafe { &*(ptr::from_ref::<[MaybeUninit<u8>]>(maybe_uninit) as *const UninitSlice) }
                }
            }

            impl IndexMut<$t> for UninitSlice {
                #[inline]
                fn index_mut(&mut self, index: $t) -> &mut UninitSlice {
                    let maybe_uninit: &mut [MaybeUninit<u8>] = &mut self.0[index];
                    unsafe { &mut *(ptr::from_mut::<[MaybeUninit<u8>]>(maybe_uninit) as *mut UninitSlice) }
                }
            }
        )*
    };
}

impl_index!(
    Range<usize>,
    RangeFrom<usize>,
    RangeFull,
    RangeInclusive<usize>,
    RangeTo<usize>,
    RangeToInclusive<usize>
);
