use std::{cell::Cell, panic, ptr};

thread_local! {
    static CB: Cell<*const Callbacks> = const { Cell::new(ptr::null()) };
}

struct Callbacks {
    before: Box<dyn Fn() -> Option<*const ()>>,
    enter: Box<dyn Fn(*const ()) -> *const ()>,
    exit: Box<dyn Fn(*const ())>,
    after: Box<dyn Fn(*const ())>,
}

pub(crate) struct Data {
    cb: &'static Callbacks,
    ptr: *const (),
}

impl Data {
    pub(crate) fn load() -> Option<Data> {
        let cb = CB.with(|cb| cb.get());

        if let Some(cb) = unsafe { cb.as_ref() }
            && let Some(ptr) = (*cb.before)()
        {
            return Some(Data { cb, ptr });
        }
        None
    }

    pub(crate) fn run<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let ptr = (*self.cb.enter)(self.ptr);
        let result = f();
        (*self.cb.exit)(ptr);
        result
    }
}

impl Drop for Data {
    fn drop(&mut self) {
        (*self.cb.after)(self.ptr)
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
    before: FBefore,
    enter: FEnter,
    exit: FExit,
    after: FAfter,
) where
    FBefore: Fn() -> Option<*const ()> + 'static,
    FEnter: Fn(*const ()) -> *const () + 'static,
    FExit: Fn(*const ()) + 'static,
    FAfter: Fn(*const ()) + 'static,
{
    CB.with(|cb| {
        if !cb.get().is_null() {
            panic!("Spawn callbacks already set");
        }

        let new: *mut Callbacks = Box::leak(Box::new(Callbacks {
            before: Box::new(before),
            enter: Box::new(enter),
            exit: Box::new(exit),
            after: Box::new(after),
        }));
        cb.replace(new);
    });
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
    before: FBefore,
    enter: FEnter,
    exit: FExit,
    after: FAfter,
) -> bool
where
    FBefore: Fn() -> Option<*const ()> + 'static,
    FEnter: Fn(*const ()) -> *const () + 'static,
    FExit: Fn(*const ()) + 'static,
    FAfter: Fn(*const ()) + 'static,
{
    CB.with(|cb| {
        if !cb.get().is_null() {
            false
        } else {
            unsafe {
                task_callbacks(before, enter, exit, after);
            }
            true
        }
    })
}
