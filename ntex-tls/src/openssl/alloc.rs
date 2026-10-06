/// Route OpenSSL memory allocations to the Rust global allocator.
///
/// OpenSSL allocates with the C runtime `malloc` by default. After this call
/// it allocates through [`std::alloc`], so an application that registers
/// a `#[global_allocator]` (for example mimalloc) uses it for tls buffers
/// and handshake state as well.
///
/// The setting is process wide and OpenSSL accepts it only before its first
/// allocation, call this at the start of `main` before any other OpenSSL use.
/// Returns `false` if OpenSSL has already allocated memory or the linked library
/// does not support custom allocators (OpenSSL older than 1.1.0, `LibreSSL`,
/// `BoringSSL`, `aws-lc`), OpenSSL keeps using `malloc` in that case.
pub fn use_global_allocator() -> bool {
    #[cfg(ntex_openssl_mem)]
    {
        shim::install()
    }
    #[cfg(not(ntex_openssl_mem))]
    {
        false
    }
}

#[cfg(ntex_openssl_mem)]
mod shim {
    use std::alloc::{self, Layout};
    use std::ffi::{c_char, c_int, c_void};
    use std::ptr;

    // Every block starts with a header that keeps the requested size, `free`
    // has no size argument but `dealloc` needs the layout. 16 bytes keep the
    // returned pointer aligned like `malloc` does (`max_align_t`).
    const ALIGN: usize = 16;
    const HEADER: usize = 16;

    type MallocFn = unsafe extern "C" fn(usize, *const c_char, c_int) -> *mut c_void;
    type ReallocFn = unsafe extern "C" fn(*mut c_void, usize, *const c_char, c_int) -> *mut c_void;
    type FreeFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);

    unsafe extern "C" {
        fn CRYPTO_set_mem_functions(m: MallocFn, r: ReallocFn, f: FreeFn) -> c_int;
    }

    pub(super) fn install() -> bool {
        unsafe { CRYPTO_set_mem_functions(malloc, realloc, free) == 1 }
    }

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, ALIGN).ok()
    }

    /// Start and layout of a block returned by `malloc` or `realloc`
    #[allow(clippy::cast_ptr_alignment)] // blocks are ALIGN aligned
    unsafe fn block(p: *mut c_void) -> (*mut u8, Layout) {
        unsafe {
            let base = p.cast::<u8>().sub(HEADER);
            let size = base.cast::<usize>().read();
            (
                base,
                Layout::from_size_align_unchecked(size + HEADER, ALIGN),
            )
        }
    }

    #[allow(clippy::cast_ptr_alignment)] // blocks are ALIGN aligned
    unsafe fn finish(base: *mut u8, size: usize) -> *mut c_void {
        if base.is_null() {
            return ptr::null_mut();
        }
        unsafe {
            base.cast::<usize>().write(size);
            base.add(HEADER).cast()
        }
    }

    pub(super) unsafe extern "C" fn malloc(size: usize, _: *const c_char, _: c_int) -> *mut c_void {
        // OpenSSL's own malloc returns null for empty allocations
        if size == 0 {
            return ptr::null_mut();
        }
        match layout(size) {
            Some(layout) => unsafe { finish(alloc::alloc(layout), size) },
            None => ptr::null_mut(),
        }
    }

    pub(super) unsafe extern "C" fn realloc(
        p: *mut c_void,
        size: usize,
        file: *const c_char,
        line: c_int,
    ) -> *mut c_void {
        // OpenSSL forwards these cases to a custom realloc unchanged
        if p.is_null() {
            return unsafe { malloc(size, file, line) };
        }
        if size == 0 {
            unsafe { free(p, file, line) };
            return ptr::null_mut();
        }
        let Some(new) = layout(size) else {
            return ptr::null_mut();
        };
        unsafe {
            let (base, old) = block(p);
            // on failure the original block stays valid, as with C realloc
            finish(alloc::realloc(base, old, new.size()), size)
        }
    }

    pub(super) unsafe extern "C" fn free(p: *mut c_void, _: *const c_char, _: c_int) {
        if !p.is_null() {
            unsafe {
                let (base, layout) = block(p);
                alloc::dealloc(base, layout);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn malloc_realloc_free() {
            let (file, line) = (ptr::null(), 0);
            unsafe {
                assert!(malloc(0, file, line).is_null());
                assert!(malloc(usize::MAX - 8, file, line).is_null());
                free(ptr::null_mut(), file, line);

                let p = malloc(3, file, line).cast::<u8>();
                assert!(!p.is_null());
                assert_eq!(p as usize % ALIGN, 0);
                p.copy_from(b"abc".as_ptr(), 3);

                // grow keeps the data
                let p = realloc(p.cast(), 64 * 1024, file, line).cast::<u8>();
                assert_eq!(p as usize % ALIGN, 0);
                assert_eq!(std::slice::from_raw_parts(p, 3), b"abc");
                p.add(64 * 1024 - 1).write(1);

                // shrink keeps the prefix
                let p = realloc(p.cast(), 2, file, line).cast::<u8>();
                assert_eq!(std::slice::from_raw_parts(p, 2), b"ab");

                // too large fails and keeps the block
                assert!(realloc(p.cast(), usize::MAX - 8, file, line).is_null());
                assert_eq!(std::slice::from_raw_parts(p, 2), b"ab");

                // zero size frees
                assert!(realloc(p.cast(), 0, file, line).is_null());

                // null acts as malloc
                let p = realloc(ptr::null_mut(), 5, file, line);
                assert!(!p.is_null());
                free(p, file, line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tls_openssl::ssl::{SslConnector, SslMethod};

    #[test]
    fn rejected_after_openssl_allocated() {
        drop(SslConnector::builder(SslMethod::tls()).unwrap());
        assert!(!super::use_global_allocator());
    }
}
