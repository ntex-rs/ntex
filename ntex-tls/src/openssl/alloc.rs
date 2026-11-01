//! Optional process-wide bridge from OpenSSL's allocation hooks to Rust's allocator.

/// Routes OpenSSL memory allocations to the Rust global allocator.
///
/// OpenSSL allocates with the C runtime `malloc` by default. After this call
/// it allocates through [`std::alloc`], so an application that registers
/// a `#[global_allocator]` (for example mimalloc) uses it for TLS buffers
/// and handshake state as well.
///
/// The setting is process wide and OpenSSL accepts it only before its first
/// allocation, call this at the start of `main` before any other OpenSSL use.
/// Returns `false` if OpenSSL has already allocated memory or the linked library
/// does not support custom allocators (OpenSSL older than 1.1.0, `LibreSSL`,
/// `BoringSSL`, `aws-lc`). A failed call leaves the existing allocator unchanged,
/// which may already be a custom allocator installed by another caller.
///
/// Returns `true` when the hooks were installed. This affects all OpenSSL users
/// in the process, not just connections created by this crate.
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
    //! C allocation callbacks with an aligned size header for Rust deallocation.
    use std::alloc::{self, Layout};
    use std::ffi::{c_char, c_int, c_void};
    use std::ptr;

    // Every block starts with a header that keeps the requested size, `free`
    // has no size argument but `dealloc` needs the layout. 16 bytes keep the
    // returned pointer aligned like `malloc` does (`max_align_t`).
    /// Alignment of both the allocation base and the returned user pointer.
    const ALIGN: usize = 16;
    /// Prefix storing the requested size while preserving user-pointer alignment.
    const HEADER: usize = 16;

    /// OpenSSL allocation callback, including ignored source-location arguments.
    type MallocFn = unsafe extern "C" fn(usize, *const c_char, c_int) -> *mut c_void;
    /// OpenSSL reallocation callback.
    type ReallocFn = unsafe extern "C" fn(*mut c_void, usize, *const c_char, c_int) -> *mut c_void;
    /// OpenSSL deallocation callback.
    type FreeFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);

    unsafe extern "C" {
        /// Installs callbacks only while OpenSSL still permits allocator replacement.
        fn CRYPTO_set_mem_functions(m: MallocFn, r: ReallocFn, f: FreeFn) -> c_int;
    }

    /// Attempts to install this shim; failure leaves the existing hooks intact.
    pub(super) fn install() -> bool {
        unsafe { CRYPTO_set_mem_functions(malloc, realloc, free) == 1 }
    }

    /// Includes the aligned header, rejecting arithmetic overflow and invalid layouts.
    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, ALIGN).ok()
    }

    /// Recovers the allocation base and layout from a returned user pointer.
    ///
    /// # Safety
    ///
    /// `p` must be a non-null, live pointer returned by this shim's `malloc` or
    /// `realloc`. Its preceding size header must remain intact.
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

    /// Stores the requested size and returns the user pointer, preserving null on failure.
    ///
    /// # Safety
    ///
    /// A non-null `base` must own an allocation with the layout returned by
    /// `layout(size)`, and its header must be exclusively writable.
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

    /// Allocates a block, returning null for zero size, overflow, or allocation failure.
    ///
    /// # Safety
    ///
    /// Returned storage must be released or resized only through this shim.
    /// Source-location arguments are ignored and are not dereferenced.
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

    /// Resizes a block with C semantics, preserving the original allocation on failure.
    ///
    /// A null pointer acts as allocation; zero size frees a non-null pointer.
    ///
    /// # Safety
    ///
    /// A non-null `p` must be an exclusively owned live pointer from this shim
    /// with an intact header. After success, only the returned pointer is valid.
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

    /// Releases an allocation; a null pointer is a no-op.
    ///
    /// # Safety
    ///
    /// A non-null `p` must be an exclusively owned live pointer from this shim
    /// with an intact header, and must not be used after this call.
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

        /// STATEMENT: The allocator shim preserves alignment, data, and C allocation semantics.
        ///
        /// HOW:
        /// - Allocate, grow, and shrink a block, checking alignment and retained bytes.
        /// - Check zero-size and overflowing requests, null-pointer operations, and that
        ///   a failed reallocation leaves the original bytes intact.
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

    /// STATEMENT: Allocator replacement is rejected after OpenSSL has allocated memory.
    ///
    /// HOW:
    /// - Construct and drop an `SslConnector` builder.
    /// - Confirm `use_global_allocator` returns `false`.
    #[test]
    fn rejected_after_openssl_allocated() {
        drop(SslConnector::builder(SslMethod::tls()).unwrap());
        assert!(!super::use_global_allocator());
    }
}
