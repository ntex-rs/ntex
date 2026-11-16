//! Typed, shared configuration for services.
//!
//! [`SharedCfg`] stores configuration values by their concrete type. Start
//! with [`SharedCfg::new`], insert values through [`SharedCfgBuilder::add`],
//! and finish with [`SharedCfgBuilder::build`]. Adding the same type again
//! replaces its previous value.
//!
//! Each value implements [`Configuration`] and stores a [`CfgContext`].
//! The builder associates that context with the collection, providing access
//! to its tag, service name, and other configuration types.
//! [`SharedCfg::get`] returns a [`Cfg<T>`] handle that dereferences to `T`.
//! If a type was not inserted, lookup creates its default value and caches it
//! for that collection on the calling thread.
//!
//! The built collection can be shared across threads, while typed [`Cfg`]
//! handles are local to the thread that obtains them. Configuration is
//! separate from the state passed to [`crate::Service`] operations; a state
//! value can carry whichever configuration handles the service needs.
//!
//! # Example
//!
//! ```
//! use ntex_service::cfg::{CfgContext, Configuration, SharedCfg};
//!
//! #[derive(Debug, Default)]
//! struct Limits {
//!     ctx: CfgContext,
//!     max_requests: usize,
//! }
//!
//! impl Configuration for Limits {
//!     const NAME: &'static str = "limits";
//!
//!     fn ctx(&self) -> &CfgContext {
//!         &self.ctx
//!     }
//!
//!     fn set_ctx(&mut self, ctx: CfgContext) {
//!         self.ctx = ctx;
//!     }
//! }
//!
//! let shared = SharedCfg::new("listener")
//!     .service("example")
//!     .add(Limits {
//!         max_requests: 32,
//!         ..Limits::default()
//!     })
//!     .build();
//! let limits = shared.get::<Limits>();
//! assert_eq!(limits.max_requests, 32);
//! assert_eq!(limits.tag(), "listener");
//! assert_eq!(limits.service(), "example");
//! ```
//!
//! # Framework guide
//!
//! See [Service State](https://docs.ntex.rs/3-state.html) for passing shared
//! data to services.

#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::any::{Any, TypeId};
use std::cell::{RefCell, UnsafeCell};
use std::sync::{Arc, LazyLock, atomic::AtomicUsize, atomic::Ordering};
use std::{fmt, hash::Hash, hash::Hasher, marker::PhantomData, mem, ops, ptr, rc};

type Key = (usize, TypeId);
type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

// Process-wide and never dropped: a default `CfgContext` holds a raw pointer to
// it and can be sent to, and outlive, any thread.
static DEFAULT_CFG: LazyLock<Arc<Storage>> = LazyLock::new(|| {
    let mut st = Arc::new(Storage::new("--", "", false, CfgContext(ptr::null())));
    let p = Arc::as_ptr(&st);
    Arc::get_mut(&mut st).unwrap().ctx.update(p);
    st
});

thread_local! {
    static MAPPING: RefCell<HashMap<Key, Arc<dyn Any + Send + Sync>>> = {
        RefCell::new(HashMap::default())
    };
}
static IDX: AtomicUsize = AtomicUsize::new(0);

/// A value that can be stored in [`SharedCfg`].
///
/// [`Default`] supplies the value returned when a shared configuration does
/// not contain this type; see [`SharedCfg::get`]. Each thread creates its own
/// default, so it should be deterministic and must not stand in for a value
/// the application is required to provide.
pub trait Configuration: Default + Send + Sync + fmt::Debug + 'static {
    /// Human-readable configuration name used in diagnostics.
    const NAME: &'static str;

    /// Returns the shared context associated with this value.
    fn ctx(&self) -> &CfgContext;

    /// Associates this value with a shared configuration context.
    fn set_ctx(&mut self, ctx: CfgContext);
}

#[derive(Debug)]
struct Storage {
    id: usize,
    tag: &'static str,
    service: &'static str,
    ctx: CfgContext,
    building: bool,
    data: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Storage {
    fn new(tag: &'static str, service: &'static str, building: bool, ctx: CfgContext) -> Self {
        let id = IDX.fetch_add(1, Ordering::SeqCst);
        Storage {
            id,
            ctx,
            tag,
            service,
            building,
            data: HashMap::default(),
        }
    }
}

#[derive(Debug)]
/// Metadata and lookup access for a shared configuration.
pub struct CfgContext(*const Storage);

// SAFETY: The pointer targets an `Arc<Storage>`, and `Storage` is `Send + Sync`.
unsafe impl Send for CfgContext {}
// SAFETY: Only shared access to `Storage` is exposed, and `Storage` is `Sync`.
unsafe impl Sync for CfgContext {}

impl CfgContext {
    fn update(&mut self, new_p: *const Storage) {
        self.0 = new_p;
    }

    /// Returns the unique identifier of the shared configuration.
    pub fn id(&self) -> usize {
        self.get_ref().id
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.get_ref().tag
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.get_ref().service
    }

    /// Returns a typed configuration value from the same shared configuration.
    ///
    /// A missing value falls back to a per-thread default, as described in
    /// [`SharedCfg::get`].
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        // SAFETY: `self.0` comes from `Arc::as_ptr` of a live `Arc<Storage>`;
        // `mem::forget` below keeps the strong count unchanged.
        let inner: Arc<Storage> = unsafe { Arc::from_raw(self.0) };
        let cfg = get(&inner);
        mem::forget(inner);
        cfg
    }

    /// Returns a handle to the complete shared configuration.
    pub fn shared(&self) -> SharedCfg {
        // SAFETY: `self.0` comes from `Arc::as_ptr` of a live `Arc<Storage>`;
        // `mem::forget` below keeps the strong count unchanged.
        let inner: Arc<Storage> = unsafe { Arc::from_raw(self.0) };
        let shared = SharedCfg(inner.clone());
        mem::forget(inner);
        shared
    }

    fn get_ref(&self) -> &Storage {
        // SAFETY: `self.0` points to the never-dropped default storage, or to a
        // built storage kept alive by the `Cfg` that exposes this context.
        unsafe { self.0.as_ref().unwrap() }
    }
}

impl Default for CfgContext {
    #[inline]
    fn default() -> Self {
        CfgContext(Arc::as_ptr(&DEFAULT_CFG))
    }
}

/// The handles owned by a [`Cfg`].
///
/// The storage handle is the one `get` cloned, so releasing it never depends
/// on the user's [`Configuration::ctx`].
struct CfgRef<T> {
    val: Arc<T>,
    storage: Arc<Storage>,
}

#[derive(Debug)]
/// A typed handle to a value in a [`SharedCfg`].
pub struct Cfg<T: Configuration>(UnsafeCell<CfgRef<T>>, PhantomData<rc::Rc<T>>);

impl<T: Configuration> Cfg<T> {
    fn new(val: Arc<T>, storage: Arc<Storage>) -> Self {
        Self(UnsafeCell::new(CfgRef { val, storage }), PhantomData)
    }

    #[inline]
    /// Returns the unique identifier of the shared configuration.
    pub fn id(&self) -> usize {
        self.inner().storage.id
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.inner().storage.tag
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.inner().storage.service
    }

    /// Returns a handle to the complete shared configuration.
    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.inner().storage.clone())
    }

    /// Returns another typed value from the same shared configuration.
    ///
    /// A missing value falls back to a per-thread default, as described in
    /// [`SharedCfg::get`].
    pub fn get_shared<U>(&self) -> Cfg<U>
    where
        U: Configuration,
    {
        get(&self.inner().storage)
    }

    fn inner(&self) -> &CfgRef<T> {
        // SAFETY: Only `replace` writes the cell, and its caller guarantees that
        // no references into it exist.
        unsafe { &*self.0.get() }
    }

    #[allow(clippy::needless_pass_by_value)]
    /// Replaces the inner value.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that no references to the inner `T` value
    /// exist at the time this function is called.
    pub unsafe fn replace(&self, cfg: Cfg<T>) {
        // SAFETY: The caller guarantees no references to the inner value exist.
        unsafe {
            ptr::swap(self.0.get(), cfg.0.get());
        }
    }
}

impl<T: Configuration> Clone for Cfg<T> {
    #[inline]
    fn clone(&self) -> Self {
        let inner = self.inner();
        Self::new(inner.val.clone(), inner.storage.clone())
    }
}

/// Wraps a standalone value in a new shared configuration tagged `--`.
///
/// There is no conversion from `&T`: a borrowed value cannot be moved into a
/// configuration. Pass the value by value, clone an existing [`Cfg`] handle,
/// or look up the registered value with `value.ctx().get::<T>()`.
impl<T: Configuration> From<T> for Cfg<T> {
    #[inline]
    fn from(cfg: T) -> Self {
        SharedCfg::new("--").add(cfg).build().get()
    }
}

impl<T: Configuration> ops::Deref for Cfg<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner().val
    }
}

impl<T: Configuration> Default for Cfg<T> {
    #[inline]
    fn default() -> Self {
        SharedCfg::default().get()
    }
}

#[derive(Clone, Debug)]
/// An immutable collection of typed configuration values.
pub struct SharedCfg(Arc<Storage>);

#[derive(Debug)]
/// Builder for [`SharedCfg`].
pub struct SharedCfgBuilder {
    ctx: CfgContext,
    storage: Arc<Storage>,
}

impl Eq for SharedCfg {}

impl PartialEq for SharedCfg {
    fn eq(&self, other: &Self) -> bool {
        ptr::from_ref(self.0.as_ref()) == ptr::from_ref(other.0.as_ref())
    }
}

impl Hash for SharedCfg {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl SharedCfg {
    /// Starts building a shared configuration with the given tag.
    pub fn new(tag: &'static str) -> SharedCfgBuilder {
        SharedCfgBuilder::new(tag)
    }

    #[inline]
    /// Returns the unique identifier of this configuration.
    pub fn id(&self) -> usize {
        self.0.id
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.0.tag
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.0.service
    }

    /// Returns the configuration value of type `T`, or its default.
    ///
    /// A value added with [`SharedCfgBuilder::add`] is the same instance on
    /// every thread. When `T` was not added, the first lookup on a thread
    /// creates `T::default()`, logs it at `info` level, and caches it for this
    /// shared configuration on that thread. Later lookups on the same thread,
    /// including through [`CfgContext::get`] and [`Cfg::get_shared`], return
    /// the cached value; other threads create their own.
    ///
    /// A missing value is not an error, and the returned handle does not show
    /// whether the value was added or defaulted. Framework components rely on
    /// this to fall back to their defaults. Add configuration your application
    /// requires explicitly, and validate it where you build the
    /// [`SharedCfg`].
    ///
    /// # Panics
    ///
    /// Panics if the shared configuration is still being built.
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        get(&self.0)
    }
}

impl Default for SharedCfg {
    #[inline]
    fn default() -> Self {
        Self(Arc::clone(&DEFAULT_CFG))
    }
}

impl<'a> From<&'a SharedCfg> for SharedCfg {
    fn from(cfg: &'a SharedCfg) -> SharedCfg {
        cfg.clone()
    }
}

impl<T: Configuration> From<SharedCfg> for Cfg<T> {
    #[inline]
    fn from(cfg: SharedCfg) -> Self {
        cfg.get()
    }
}

impl<'a, T: Configuration> From<&'a SharedCfg> for Cfg<T> {
    #[inline]
    fn from(cfg: &'a SharedCfg) -> Self {
        cfg.get()
    }
}

impl<T: Configuration> From<SharedCfgBuilder> for Cfg<T> {
    #[inline]
    fn from(cfg: SharedCfgBuilder) -> Self {
        cfg.build().get()
    }
}

impl<T: Configuration> From<T> for SharedCfg {
    #[inline]
    fn from(cfg: T) -> Self {
        SharedCfg::new("--").add(cfg).build()
    }
}

impl<T: Configuration> From<Cfg<T>> for SharedCfg {
    #[inline]
    fn from(cfg: Cfg<T>) -> Self {
        cfg.shared()
    }
}

impl SharedCfgBuilder {
    fn new(tag: &'static str) -> SharedCfgBuilder {
        let mut storage = Arc::new(Storage::new(tag, tag, true, CfgContext::default()));
        let ctx = CfgContext(Arc::as_ptr(&storage));
        Arc::get_mut(&mut storage).unwrap().ctx.update(ctx.0);

        SharedCfgBuilder { ctx, storage }
    }

    #[must_use]
    /// Sets the service name.
    pub fn service(mut self, name: &'static str) -> Self {
        Arc::get_mut(&mut self.storage).unwrap().service = name;
        self
    }

    #[must_use]
    /// Inserts a typed value into this configuration.
    ///
    /// Replaces the existing value when the same type was already inserted.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(CfgContext(self.ctx.0));
        Arc::get_mut(&mut self.storage)
            .unwrap()
            .data
            .insert(TypeId::of::<T>(), Arc::new(val));
        self
    }

    #[must_use]
    /// Finishes building and returns the shared configuration.
    pub fn build(self) -> SharedCfg {
        self.into()
    }
}

impl From<SharedCfgBuilder> for SharedCfg {
    fn from(mut cfg: SharedCfgBuilder) -> SharedCfg {
        let st = Arc::get_mut(&mut cfg.storage).unwrap();
        st.building = false;
        SharedCfg(cfg.storage)
    }
}

fn get<T>(st: &Arc<Storage>) -> Cfg<T>
where
    T: Configuration,
{
    assert!(
        !st.building,
        "{}: Cannot access shared config while building",
        st.tag
    );

    let tp = TypeId::of::<T>();
    let val = if let Some(val) = st.data.get(&tp) {
        val.clone()
    } else {
        MAPPING.with(|store| {
            let key = (st.id, tp);
            if let Some(val) = store.borrow().get(&key) {
                val.clone()
            } else {
                log::info!(
                    "{}: Configuration {:?} does not exist, using default",
                    st.tag,
                    T::NAME
                );
                let mut val = T::default();
                val.set_ctx(CfgContext(st.ctx.0));
                let val: Arc<dyn Any + Send + Sync> = Arc::new(val);
                store.borrow_mut().insert(key, val.clone());
                val
            }
        })
    };
    // values are keyed by their `TypeId`
    Cfg::new(val.downcast::<T>().unwrap(), st.clone())
}

#[cfg(test)]
#[path = "../tests/unit/cfg.rs"]
mod tests;
