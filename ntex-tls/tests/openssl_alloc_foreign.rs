//! `use_global_allocator()` refuses to replace foreign OpenSSL memory hooks.
//!
//! The hooks are process wide, so this binary contains a single test.
#![cfg(feature = "openssl")]
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

use ntex_tls::openssl::use_global_allocator;

type MallocFn = unsafe extern "C" fn(usize, *const c_char, c_int) -> *mut c_void;
type ReallocFn = unsafe extern "C" fn(*mut c_void, usize, *const c_char, c_int) -> *mut c_void;
type FreeFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int);

unsafe extern "C" {
    fn CRYPTO_set_mem_functions(m: MallocFn, r: ReallocFn, f: FreeFn) -> c_int;
    fn CRYPTO_get_mem_functions(
        m: *mut Option<MallocFn>,
        r: *mut Option<ReallocFn>,
        f: *mut Option<FreeFn>,
    );
}

static FREES: AtomicUsize = AtomicUsize::new(0);

/// Foreign allocation hook backed by the C allocator.
unsafe extern "C" fn foreign_malloc(size: usize, _: *const c_char, _: c_int) -> *mut c_void {
    // SAFETY: `malloc` accepts the requested size without additional pointer preconditions.
    unsafe { libc::malloc(size) }
}

/// Foreign reallocation hook backed by the C allocator.
unsafe extern "C" fn foreign_realloc(
    p: *mut c_void,
    size: usize,
    _: *const c_char,
    _: c_int,
) -> *mut c_void {
    // SAFETY: OpenSSL passes a null pointer or a live block from these C-allocator hooks.
    unsafe { libc::realloc(p, size) }
}

/// Foreign deallocation hook backed by the C allocator, counting calls.
unsafe extern "C" fn foreign_free(p: *mut c_void, _: *const c_char, _: c_int) {
    FREES.fetch_add(1, Ordering::Relaxed);
    // SAFETY: OpenSSL passes a null pointer or a live block from these C-allocator hooks, freed
    // once.
    unsafe { libc::free(p) }
}

/// STATEMENT: Foreign OpenSSL memory hooks are left in place even though OpenSSL
/// would still accept a replacement.
///
/// HOW:
/// - Install C-allocator hooks before OpenSSL use, then allocate through OpenSSL.
/// - Confirm `CRYPTO_set_mem_functions` still accepts the foreign hooks, so only
///   the hook check prevents replacement.
/// - Confirm `use_global_allocator` returns `false`, the installed hooks are unchanged,
///   and freeing the earlier block goes through the foreign hook.
#[test]
fn foreign_hooks_are_not_replaced() {
    // SAFETY: This sole test installs hooks before OpenSSL use; hook queries use valid output
    // slots.
    // The allocated block is freed once through the same unchanged C-allocator hooks.
    unsafe {
        assert_eq!(
            CRYPTO_set_mem_functions(foreign_malloc, foreign_realloc, foreign_free),
            1
        );
        let p = openssl_sys::CRYPTO_malloc(64, c"test".as_ptr(), 0);
        assert!(!p.is_null());
        assert_eq!(
            CRYPTO_set_mem_functions(foreign_malloc, foreign_realloc, foreign_free),
            1
        );

        // No other thread uses OpenSSL.
        assert!(!use_global_allocator());

        let (mut m, mut r, mut f) = (None, None, None);
        CRYPTO_get_mem_functions(&raw mut m, &raw mut r, &raw mut f);
        assert_eq!(
            m.map(|m| m as usize),
            Some(foreign_malloc as MallocFn as usize)
        );
        assert_eq!(
            r.map(|r| r as usize),
            Some(foreign_realloc as ReallocFn as usize)
        );
        assert_eq!(f.map(|f| f as usize), Some(foreign_free as FreeFn as usize));

        openssl_sys::CRYPTO_free(p, c"test".as_ptr(), 0);
        assert_eq!(FREES.load(Ordering::Relaxed), 1);
    }
}
