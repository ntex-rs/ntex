use crate::{ByteString, Bytes, storage::Storage};

/// External storage type vtable
#[derive(Debug)]
pub struct StorageVTable {
    pub(crate) as_ptr: unsafe fn(*const u8, usize) -> *const u8,
    pub(crate) len: unsafe fn(*const u8, usize) -> usize,
    pub(crate) clone: unsafe fn(*const u8, usize) -> Option<(*const u8, usize)>,
    pub(crate) drop: unsafe fn(*const u8, usize),
}

impl StorageVTable {
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

/// Types that could be used as external storage for Bytes
pub trait StorageExt: Send + Sync {
    fn create(self) -> (*const u8, usize, &'static StorageVTable);
}

/// Type's `as_ptr` must return ptr to valid string.
///
/// # Safety
/// Only valid str could implement this trait
pub unsafe trait StorageExtStr: StorageExt + Sized {
    fn create(self) -> (*const u8, usize, &'static StorageVTable) {
        StorageExt::create(self)
    }
}

impl Bytes {
    pub fn from_ext<T: StorageExt>(val: T) -> Bytes {
        let (addr, len, vtable) = val.create();

        Bytes {
            storage: Storage::from_stext(addr, len, vtable),
        }
    }
}

impl ByteString {
    pub fn from_ext<T: StorageExtStr>(val: T) -> ByteString {
        let (addr, len, vtable) = StorageExtStr::create(val);

        unsafe {
            ByteString::from_bytes_unchecked(Bytes {
                storage: Storage::from_stext(addr, len, vtable),
            })
        }
    }
}
