use std::{sync::Arc, sync::atomic::AtomicUsize, sync::atomic::Ordering};

// The Callbacks static holds a pointer to the global logger. It is protected by
// the STATE static which determines whether LOGGER has been initialized yet.
static mut CBS: Option<Arc<dyn CallbacksApi>> = None;

static STATE: AtomicUsize = AtomicUsize::new(0);

// There are three different states that we care about: the logger's
// uninitialized, the logger's initializing (set_logger's been called but
// LOGGER hasn't actually been set yet), or the logger's active.
const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

trait CallbacksApi {
    fn before(&self) -> Option<*const ()>;
    fn enter(&self, _: *const ()) -> *const ();
    fn exit(&self, _: *const ());
    fn after(&self, _: *const ());
}

#[allow(clippy::struct_field_names)]
struct Callbacks<A, B, C, D> {
    f_before: A,
    f_enter: B,
    f_exit: C,
    f_after: D,
}

impl<A, B, C, D> CallbacksApi for Callbacks<A, B, C, D>
where
    A: Fn() -> Option<*const ()> + 'static,
    B: Fn(*const ()) -> *const () + 'static,
    C: Fn(*const ()) + 'static,
    D: Fn(*const ()) + 'static,
{
    fn before(&self) -> Option<*const ()> {
        (self.f_before)()
    }
    fn enter(&self, d: *const ()) -> *const () {
        (self.f_enter)(d)
    }
    fn exit(&self, d: *const ()) {
        (self.f_exit)(d);
    }
    fn after(&self, d: *const ()) {
        (self.f_after)(d);
    }
}

pub(crate) struct Data {
    cb: &'static dyn CallbacksApi,
    ptr: *const (),
}

impl Data {
    #[allow(clippy::if_not_else)]
    pub(crate) fn load() -> Option<Data> {
        // Acquire memory ordering guarantees that current thread would see any
        // memory writes that happened before store of the value
        // into `STATE` with memory ordering `Release` or stronger.
        let cb = if STATE.load(Ordering::Acquire) != INITIALIZED {
            None
        } else {
            #[allow(static_mut_refs)]
            unsafe {
                Some(CBS.as_ref().map(AsRef::as_ref).unwrap())
            }
        };

        if let Some(cb) = cb
            && let Some(ptr) = cb.before()
        {
            return Some(Data { cb, ptr });
        }
        None
    }

    pub(crate) fn run<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let ptr = self.cb.enter(self.ptr);
        let result = f();
        self.cb.exit(ptr);
        result
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        self.cb.after(self.ptr);
    }
}

/// # Safety
///
/// The user must ensure that the pointer returned by `before` has a `'static` lifetime.
/// This pointer will be owned by the spawned task for the duration of that task, and
/// ownership will be returned to the user at the end of the task via `after`.
/// The pointer remains opaque to the runtime.
///
/// # Panics
///
/// Panics if task callbacks have already been set.
pub unsafe fn task_callbacks<FBefore, FEnter, FExit, FAfter>(
    f_before: FBefore,
    f_enter: FEnter,
    f_exit: FExit,
    f_after: FAfter,
) where
    FBefore: Fn() -> Option<*const ()> + 'static + Sync,
    FEnter: Fn(*const ()) -> *const () + 'static + Sync,
    FExit: Fn(*const ()) + 'static + Sync,
    FAfter: Fn(*const ()) + 'static + Sync,
{
    let new = Arc::new(Callbacks {
        f_before,
        f_enter,
        f_exit,
        f_after,
    });
    let _ = set_cbs(new);
}

/// # Safety
///
/// The user must ensure that the pointer returned by `before` has a `'static` lifetime.
/// This pointer will be owned by the spawned task for the duration of that task, and
/// ownership will be returned to the user at the end of the task via `after`.
/// The pointer remains opaque to the runtime.
///
/// Returns false if task callbacks have already been set.
pub unsafe fn task_opt_callbacks<FBefore, FEnter, FExit, FAfter>(
    f_before: FBefore,
    f_enter: FEnter,
    f_exit: FExit,
    f_after: FAfter,
) -> bool
where
    FBefore: Fn() -> Option<*const ()> + Sync + 'static,
    FEnter: Fn(*const ()) -> *const () + Sync + 'static,
    FExit: Fn(*const ()) + Sync + 'static,
    FAfter: Fn(*const ()) + Sync + 'static,
{
    let new = Arc::new(Callbacks {
        f_before,
        f_enter,
        f_exit,
        f_after,
    });
    set_cbs(new).is_ok()
}

fn set_cbs(cbs: Arc<dyn CallbacksApi>) -> Result<(), ()> {
    match STATE.compare_exchange(
        UNINITIALIZED,
        INITIALIZING,
        Ordering::Acquire,
        Ordering::Relaxed,
    ) {
        Ok(UNINITIALIZED) => {
            unsafe {
                CBS = Some(cbs);
            }
            STATE.store(INITIALIZED, Ordering::Release);
            Ok(())
        }
        Err(INITIALIZING) => {
            while STATE.load(Ordering::Relaxed) == INITIALIZING {
                std::hint::spin_loop();
            }
            Err(())
        }
        _ => Err(()),
    }
}
