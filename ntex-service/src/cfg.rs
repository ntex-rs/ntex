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
//! A [`CfgContext`] keeps the id, tag, and service name of its collection
//! readable, but does not keep the collection alive. Lookups through a
//! context panic while the collection is being built and after it has been
//! dropped; [`SharedCfg`] and [`Cfg`] handles keep it alive.
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

#![forbid(unsafe_code)]
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::sync::{Arc, LazyLock, OnceLock, Weak, atomic::AtomicUsize, atomic::Ordering};
use std::{fmt, hash::Hash, hash::Hasher, marker::PhantomData, ops, rc};

type Key = (usize, TypeId);
type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
type ConfigMap = HashMap<TypeId, Arc<dyn Any + Send + Sync>>;

const DEFAULT_ID: usize = 0;
const DEFAULT_TAG: &str = "--";
const DEFAULT_SERVICE: &str = "";

// Process-wide and never dropped, so a default `CfgContext` needs no handle to
// it and can be sent to, and outlive, any thread.
static DEFAULT_CFG: LazyLock<Arc<Storage>> = LazyLock::new(|| {
    Arc::new(Storage {
        id: DEFAULT_ID,
        tag: DEFAULT_TAG,
        service: DEFAULT_SERVICE,
        ctx: CfgContext(None),
        data: ConfigMap::default(),
    })
});

thread_local! {
    static MAPPING: RefCell<HashMap<Key, Arc<dyn Any + Send + Sync>>> = {
        RefCell::new(HashMap::default())
    };
}
// `DEFAULT_ID` is reserved for the default configuration.
static IDX: AtomicUsize = AtomicUsize::new(DEFAULT_ID + 1);

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
    ///
    /// [`SharedCfgBuilder::add`] calls this before the configuration is
    /// built, so [`CfgContext::get`] and [`CfgContext::shared`] panic here;
    /// store the context and use it later.
    fn set_ctx(&mut self, ctx: CfgContext);
}

#[derive(Debug)]
struct Storage {
    id: usize,
    tag: &'static str,
    service: &'static str,
    ctx: CfgContext,
    data: ConfigMap,
}

/// The identity of a built or building configuration, shared by its builder,
/// its storage, and every context handed to its values.
#[derive(Debug)]
struct ContextInner {
    id: usize,
    tag: &'static str,
    published: OnceLock<Published>,
}

/// Set once, when the builder finishes.
#[derive(Debug)]
struct Published {
    storage: Weak<Storage>,
    service: &'static str,
}

impl ContextInner {
    fn storage(&self) -> Arc<Storage> {
        let Some(published) = self.published.get() else {
            building(self.tag)
        };
        match published.storage.upgrade() {
            Some(storage) => storage,
            None => expired(self.tag),
        }
    }
}

#[cold]
#[inline(never)]
fn building(tag: &str) -> ! {
    panic!("{tag}: Cannot access shared config while building")
}

#[cold]
#[inline(never)]
fn expired(tag: &str) -> ! {
    panic!("{tag}: Cannot access shared config after it was dropped")
}

#[derive(Debug)]
/// Metadata and lookup access for a shared configuration.
///
/// The id, tag, and service name stay readable for as long as the context
/// exists. Lookups need the configuration itself, see [`CfgContext::get`].
pub struct CfgContext(Option<Arc<ContextInner>>);

impl CfgContext {
    /// Returns the unique identifier of the shared configuration.
    pub fn id(&self) -> usize {
        self.0.as_ref().map_or(DEFAULT_ID, |inner| inner.id)
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.0.as_ref().map_or(DEFAULT_TAG, |inner| inner.tag)
    }

    /// Returns the configured service name.
    ///
    /// Returns the tag while the configuration is being built.
    pub fn service(&self) -> &'static str {
        match &self.0 {
            Some(inner) => inner.published.get().map_or(inner.tag, |p| p.service),
            None => DEFAULT_SERVICE,
        }
    }

    /// Returns a typed configuration value from the same shared configuration.
    ///
    /// A missing value falls back to a per-thread default, as described in
    /// [`SharedCfg::get`].
    ///
    /// # Panics
    ///
    /// Panics while the shared configuration is being built, for example from
    /// [`Configuration::set_ctx`] during [`SharedCfgBuilder::add`] or after its
    /// builder was dropped, and after the shared configuration was dropped.
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        get(self.storage())
    }

    /// Returns a handle to the complete shared configuration.
    ///
    /// # Panics
    ///
    /// Panics in the same cases as [`CfgContext::get`].
    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.storage())
    }

    fn storage(&self) -> Arc<Storage> {
        match &self.0 {
            Some(inner) => inner.storage(),
            None => Arc::clone(&DEFAULT_CFG),
        }
    }
}

impl Default for CfgContext {
    #[inline]
    fn default() -> Self {
        CfgContext(None)
    }
}

#[derive(Debug)]
/// A typed handle to a value in a [`SharedCfg`].
pub struct Cfg<T: Configuration> {
    value: Arc<T>,
    // The owner `get` was given, so releasing it never depends on the
    // user's `Configuration::ctx`.
    storage: Arc<Storage>,
    local: PhantomData<rc::Rc<T>>,
}

impl<T: Configuration> Cfg<T> {
    #[inline]
    /// Returns the unique identifier of the shared configuration.
    pub fn id(&self) -> usize {
        self.storage.id
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.storage.tag
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.storage.service
    }

    /// Returns a handle to the complete shared configuration.
    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.storage.clone())
    }

    /// Returns another typed value from the same shared configuration.
    ///
    /// A missing value falls back to a per-thread default, as described in
    /// [`SharedCfg::get`].
    pub fn get_shared<U>(&self) -> Cfg<U>
    where
        U: Configuration,
    {
        get(self.storage.clone())
    }
}

impl<T: Configuration> Clone for Cfg<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            storage: self.storage.clone(),
            local: PhantomData,
        }
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
        &self.value
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
    ctx: Arc<ContextInner>,
    service: &'static str,
    data: ConfigMap,
}

impl Eq for SharedCfg {}

impl PartialEq for SharedCfg {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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
    /// A panic in `T::default()` or [`Configuration::set_ctx`] while creating
    /// the default propagates to the caller, and nothing is cached.
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        get(self.0.clone())
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
        let ctx = ContextInner {
            id: IDX.fetch_add(1, Ordering::SeqCst),
            tag,
            published: OnceLock::new(),
        };
        SharedCfgBuilder {
            ctx: Arc::new(ctx),
            service: tag,
            data: ConfigMap::default(),
        }
    }

    #[must_use]
    /// Sets the service name.
    pub fn service(mut self, name: &'static str) -> Self {
        self.service = name;
        self
    }

    #[must_use]
    /// Inserts a typed value into this configuration.
    ///
    /// Replaces the existing value when the same type was already inserted.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(CfgContext(Some(self.ctx.clone())));
        self.data.insert(TypeId::of::<T>(), Arc::new(val));
        self
    }

    #[must_use]
    /// Finishes building and returns the shared configuration.
    pub fn build(self) -> SharedCfg {
        self.into()
    }
}

impl From<SharedCfgBuilder> for SharedCfg {
    fn from(cfg: SharedCfgBuilder) -> SharedCfg {
        let SharedCfgBuilder { ctx, service, data } = cfg;
        let storage = Arc::new(Storage {
            id: ctx.id,
            tag: ctx.tag,
            service,
            ctx: CfgContext(Some(ctx.clone())),
            data,
        });
        let published = ctx.published.set(Published {
            storage: Arc::downgrade(&storage),
            service,
        });
        debug_assert!(published.is_ok(), "a builder is built only once");
        SharedCfg(storage)
    }
}

/// Looks up `T` and moves `st` into the returned handle as its owner.
fn get<T>(st: Arc<Storage>) -> Cfg<T>
where
    T: Configuration,
{
    let val = match st.data.get(&TypeId::of::<T>()) {
        Some(val) => val.clone(),
        None => get_default::<T>(&st),
    };
    Cfg {
        // values are keyed by their `TypeId`
        value: val.downcast::<T>().unwrap(),
        storage: st,
        local: PhantomData,
    }
}

/// Returns the default `T` cached for `st` on this thread, creating it first.
///
/// No cache borrow is held while user code runs or a value is dropped.
fn get_default<T>(st: &Storage) -> Arc<dyn Any + Send + Sync>
where
    T: Configuration,
{
    let key = (st.id, TypeId::of::<T>());
    if let Some(val) = MAPPING.with(|store| store.borrow().get(&key).cloned()) {
        return val;
    }

    log::info!(
        "{}: Configuration {:?} does not exist, using default",
        st.tag,
        T::NAME
    );
    let mut val = T::default();
    val.set_ctx(CfgContext(st.ctx.0.clone()));
    let val: Arc<dyn Any + Send + Sync> = Arc::new(val);

    // `Default` or `set_ctx` may have cached this key already; keep that value
    // and drop ours after the borrow ends.
    let (cached, unused) = MAPPING.with(|store| match store.borrow_mut().entry(key) {
        Entry::Occupied(entry) => (entry.get().clone(), Some(val)),
        Entry::Vacant(entry) => (entry.insert(val).clone(), None),
    });
    drop(unused);
    cached
}

#[cfg(test)]
#[path = "../tests/unit/cfg.rs"]
mod tests;
