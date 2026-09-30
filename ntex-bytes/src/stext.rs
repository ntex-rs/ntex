use crate::{ByteString, Bytes, storage::Storage};

/// Functions that give [`Bytes`] access to externally owned data.
///
/// Every function receives the `(addr, len)` pair returned by
/// [`StorageExt::create`], or by a previous `clone`.
#[derive(Debug)]
pub struct StorageVTable {
    pub(crate) as_ptr: unsafe fn(*const u8, usize) -> *const u8,
    pub(crate) len: unsafe fn(*const u8, usize) -> usize,
    pub(crate) clone: unsafe fn(*const u8, usize) -> Option<(*const u8, usize)>,
    pub(crate) drop: unsafe fn(*const u8, usize),
}

impl StorageVTable {
    /// Creates a vtable from its functions.
    ///
    /// - `as_ptr` returns a pointer to the start of the data.
    /// - `len` returns the length of the data in bytes.
    /// - `clone` returns the `(addr, len)` pair of a new handle to the same
    ///   data, or `None` to make the clone copy the data instead.
    /// - `drop` releases the handle.
    ///
    /// The requirements these functions must meet are listed on
    /// [`StorageExt`].
    pub const fn new(
        as_ptr: unsafe fn(*const u8, usize) -> *const u8,
        len: unsafe fn(*const u8, usize) -> usize,
        clone: unsafe fn(*const u8, usize) -> Option<(*const u8, usize)>,
        drop: unsafe fn(*const u8, usize),
    ) -> StorageVTable {
        StorageVTable {
            as_ptr,
            len,
            clone,
            drop,
        }
    }
}

/// Types that can be used as external storage for [`Bytes`], see
/// [`Bytes::from_ext`].
///
/// # Safety
///
/// `Bytes` trusts the values returned by `create` without checking them:
///
/// - For the returned `(addr, len)` pair, and for every pair returned by the
///   vtable's `clone`, `as_ptr` and `len` must describe memory that is
///   readable for `len` bytes and stays valid and unchanged until `drop` is
///   called for that pair.
/// - The vtable functions can be called from any thread.
///
/// `Bytes` calls `drop` exactly once for each pair and does not use the pair
/// afterwards.
pub unsafe trait StorageExt: Send + Sync {
    /// Converts the value into an `(addr, len)` pair and the vtable that
    /// operates on it.
    fn create(self) -> (*const u8, usize, &'static StorageVTable);
}

/// External storage that holds valid UTF-8, see [`ByteString::from_ext`].
///
/// # Safety
///
/// The data exposed through the vtable must be valid UTF-8.
pub unsafe trait StorageExtStr: StorageExt + Sized {
    /// Same as [`StorageExt::create`].
    fn create(self) -> (*const u8, usize, &'static StorageVTable) {
        StorageExt::create(self)
    }
}

impl Bytes {
    /// Creates a `Bytes` that shares externally owned data without copying.
    ///
    /// Cloning and dropping are delegated to the value's vtable.
    pub fn from_ext<T: StorageExt>(val: T) -> Bytes {
        let (addr, len, vtable) = val.create();

        Bytes {
            storage: Storage::from_stext(addr, len, vtable),
        }
    }
}

impl ByteString {
    /// Creates a `ByteString` that shares externally owned UTF-8 data without
    /// copying.
    ///
    /// Cloning and dropping are delegated to the value's vtable.
    pub fn from_ext<T: StorageExtStr>(val: T) -> ByteString {
        let (addr, len, vtable) = StorageExtStr::create(val);

        unsafe {
            ByteString::from_bytes_unchecked(Bytes {
                storage: Storage::from_stext(addr, len, vtable),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info::Kind;

    fn as_ptr(addr: *const u8, _: usize) -> *const u8 {
        addr
    }

    fn len(_: *const u8, len: usize) -> usize {
        len
    }

    fn clone(_: *const u8, _: usize) -> Option<(*const u8, usize)> {
        None
    }

    fn drop(addr: *const u8, len: usize) {
        let ptr = std::ptr::slice_from_raw_parts_mut(addr.cast_mut(), len);
        std::mem::drop(unsafe { Box::from_raw(ptr) });
    }

    struct Boxed(Box<[u8]>);

    // SAFETY: the vtable releases the leaked box exactly once
    unsafe impl StorageExt for Boxed {
        fn create(self) -> (*const u8, usize, &'static StorageVTable) {
            static VTABLE: StorageVTable = StorageVTable::new(as_ptr, len, clone, drop);
            let len = self.0.len();
            (Box::into_raw(self.0).cast::<u8>(), len, &VTABLE)
        }
    }

    #[test]
    fn clone_copies_data() {
        let data = vec![7u8; 100].into_boxed_slice();
        let b = Bytes::from_ext(Boxed(data));
        assert_eq!(b.info().kind, Kind::StExt);

        let b2 = b.clone();
        assert_eq!(b2.info().kind, Kind::Vec);
        assert_eq!(b, b2);

        let vtable = StorageVTable::new(as_ptr, len, clone, drop);
        assert!(format!("{vtable:?}").contains("StorageVTable"));
    }
}
