use crate::{Bytes, storage::Storage};

#[derive(Debug)]
pub struct StorageVTable {
    pub(crate) as_ptr: unsafe fn(usize, usize) -> *const u8,
    pub(crate) len: unsafe fn(usize, usize) -> usize,
    pub(crate) clone: unsafe fn(usize, usize) -> (usize, usize),
    pub(crate) drop: unsafe fn(usize, usize),
}

impl StorageVTable {
    pub const fn new(
        as_ptr: unsafe fn(usize, usize) -> *const u8,
        len: unsafe fn(usize, usize) -> usize,
        clone: unsafe fn(usize, usize) -> (usize, usize),
        drop: unsafe fn(usize, usize),
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
    fn create(self) -> (usize, usize, &'static StorageVTable);
}

impl Bytes {
    pub fn from_ext<T: StorageExt>(val: T) -> Bytes {
        let (data1, data2, vtable) = val.create();

        Bytes {
            storage: Storage::from_stext(data1, data2, vtable),
        }
    }
}
