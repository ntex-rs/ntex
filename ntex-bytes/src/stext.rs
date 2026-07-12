use crate::{Bytes, storage::Storage};

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

pub trait StorageExt: Send + Sync {
    fn create(self) -> (*const u8, usize, &'static StorageVTable);
}

impl Bytes {
    pub fn from_ext<T: StorageExt>(val: T) -> Bytes {
        let (addr, len, vtable) = val.create();

        Bytes {
            storage: Storage::from_stext(addr, len, vtable),
        }
    }
}
