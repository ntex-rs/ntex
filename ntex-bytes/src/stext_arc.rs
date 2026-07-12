use std::{mem, ptr, sync::Arc};

use crate::{StorageExt, StorageExtStr, StorageVTable};

// ======== Impl for Arc<str> ========

fn as_ptr(addr: *const u8, _: usize) -> *const u8 {
    addr
}

fn len(_: *const u8, len: usize) -> usize {
    len
}

fn clone_str(addr: *const u8, len: usize) -> Option<(*const u8, usize)> {
    let slice_ptr: *const [u8] = ptr::slice_from_raw_parts(addr, len);
    let str_ptr = slice_ptr as *const str;
    let arc = unsafe { Arc::from_raw(str_ptr) };
    let arc2 = arc.clone();
    mem::forget(arc);
    mem::forget(arc2);

    Some((addr, len))
}

fn drop_str(addr: *const u8, len: usize) {
    let slice_ptr: *const [u8] = ptr::slice_from_raw_parts(addr, len);
    let str_ptr = slice_ptr as *const str;
    let arc = unsafe { Arc::from_raw(str_ptr) };
    drop(arc);
}

const ARC_STR_VTABLE: StorageVTable = StorageVTable::new(as_ptr, len, clone_str, drop_str);

impl StorageExt for Arc<str> {
    fn create(self) -> (*const u8, usize, &'static StorageVTable) {
        let ptr = Arc::into_raw(self) as *const [u8];

        // Extract address and length
        let addr = ptr.cast::<()>().cast::<u8>();
        let len = ptr.len();
        (addr, len, &ARC_STR_VTABLE)
    }
}

// # SAFETY
// Implementation must contain valid string which Arc<str> does
unsafe impl StorageExtStr for Arc<str> {}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::{ByteString, Bytes, buf::Buf};

    #[test]
    fn test_arc_str() {
        let test: Arc<str> = Arc::from("test".to_string());

        let b = Bytes::from_ext(test.clone());
        assert_eq!(&b, b"test");
        assert_eq!(b.storage.capacity(), 4);
        assert_eq!(Arc::strong_count(&test), 2);

        let b2 = b.clone();
        assert_eq!(&b2, b"test");
        assert_eq!(Arc::strong_count(&test), 3);

        drop(b2);
        assert_eq!(&b, b"test");
        assert_eq!(Arc::strong_count(&test), 2);

        drop(b);
        assert_eq!(Arc::strong_count(&test), 1);

        let b = ByteString::from_ext(test.clone());
        assert_eq!(&b, "test");
        assert_eq!(Arc::strong_count(&test), 2);

        drop(b);
        assert_eq!(Arc::strong_count(&test), 1);

        let mut b = Bytes::from_ext(test.clone());
        assert_eq!(Arc::strong_count(&test), 2);
        assert_eq!(b.get_u8(), b't');
        assert_eq!(&b, b"est");
        assert_eq!(Arc::strong_count(&test), 1);

        let mut b = Bytes::from_ext(test.clone());
        b.truncate(2);
        assert_eq!(&b, b"te");
        assert_eq!(Arc::strong_count(&test), 1);

        let mut b = Bytes::from_ext(test.clone());
        unsafe { b.storage.set_start(2) };
        assert_eq!(&b, b"st");
        assert_eq!(Arc::strong_count(&test), 1);

        let mut b = Bytes::from_ext(test.clone());
        unsafe { b.storage.set_end(2) };
        assert_eq!(&b, b"te");
        assert_eq!(Arc::strong_count(&test), 1);
    }
}
