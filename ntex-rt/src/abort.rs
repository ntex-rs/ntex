/// Aborts the process when dropped, logging the message first.
///
/// Guards code that must not unwind, for example while state that the kernel
/// or other threads still use is taken out of its owner. Declare the guard
/// after such state, so it is dropped first while unwinding, and call
/// [`disarm()`](Self::disarm) once the guarded code completed.
///
/// Bind the guard to a named variable, `let _ = AbortOnUnwind(..)` drops it,
/// and aborts, immediately.
///
/// ```rust,no_run
/// use ntex_rt::AbortOnUnwind;
///
/// let guard = AbortOnUnwind("operation panicked");
/// // code that must not unwind
/// guard.disarm();
/// ```
#[derive(Debug)]
#[must_use = "the guard aborts the process when dropped"]
pub struct AbortOnUnwind(pub &'static str);

impl AbortOnUnwind {
    /// Releases the guard without aborting.
    pub fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        log::error!("{}; aborting", self.0);
        std::process::abort();
    }
}
