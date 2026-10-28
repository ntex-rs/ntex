//! Typed, shared configuration for services.
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::any::{Any, TypeId};
use std::cell::{RefCell, UnsafeCell};
use std::sync::{Arc, OnceLock, RwLock, Weak, atomic::AtomicUsize, atomic::Ordering};
use std::{fmt, hash::Hash, hash::Hasher, marker::PhantomData, ops, ptr, rc};

type Key = (usize, TypeId);
type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

thread_local! {
    static DEFAULT_CFG: SharedCfg = SharedCfg::new("--").service("").build();
    static MAPPING: RefCell<HashMap<Key, Arc<dyn Any + Send + Sync>>> = {
        RefCell::new(HashMap::default())
    };
}
static IDX: AtomicUsize = AtomicUsize::new(0);

/// A value that can be stored in [`SharedCfg`].
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
    ctx: CfgContext,
    data: OnceLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

#[derive(Debug)]
struct Metadata {
    id: usize,
    tag: &'static str,
    service: RwLock<&'static str>,
    storage: Weak<Storage>,
}

impl Storage {
    fn new(tag: &'static str) -> Arc<Self> {
        let id = IDX.fetch_add(1, Ordering::SeqCst);
        Arc::new_cyclic(|storage| Storage {
            ctx: CfgContext(Arc::new(Metadata {
                id,
                tag,
                service: RwLock::new(tag),
                storage: storage.clone(),
            })),
            data: OnceLock::new(),
        })
    }
}

#[derive(Clone, Debug)]
/// Metadata and lookup access for a shared configuration.
///
/// A context keeps metadata alive, but does not keep configuration values alive.
/// Metadata remains readable after the last owning handle is dropped, including
/// after the thread that created a default context exits. Use [`Self::try_get`]
/// or [`Self::try_shared`] when the shared configuration may have expired.
pub struct CfgContext(Arc<Metadata>);

impl CfgContext {
    /// Returns the unique identifier of the shared configuration.
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
        *self.0.service.read().unwrap()
    }

    /// Returns a typed configuration value from the same shared configuration.
    ///
    /// # Panics
    ///
    /// Panics if the shared configuration has expired or is still being built.
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        self.shared().get()
    }

    /// Returns a typed configuration value, or `None` if the configuration expired.
    ///
    /// # Panics
    ///
    /// Panics if the shared configuration is still being built.
    pub fn try_get<T>(&self) -> Option<Cfg<T>>
    where
        T: Configuration,
    {
        self.try_shared().map(|shared| shared.get())
    }

    /// Returns a handle to the complete shared configuration.
    ///
    /// # Panics
    ///
    /// Panics if the shared configuration has expired.
    pub fn shared(&self) -> SharedCfg {
        self.try_shared()
            .unwrap_or_else(|| panic!("{}: Shared configuration has expired", self.tag()))
    }

    /// Returns an owning handle, or `None` if the shared configuration expired.
    pub fn try_shared(&self) -> Option<SharedCfg> {
        self.0.storage.upgrade().map(SharedCfg)
    }
}

impl Default for CfgContext {
    #[inline]
    fn default() -> Self {
        DEFAULT_CFG.with(|cfg| cfg.0.ctx.clone())
    }
}

#[derive(Debug)]
/// A typed handle to a value in a [`SharedCfg`].
///
/// Keeps both the value and its shared configuration alive independently of
/// the context returned by [`Configuration::ctx`].
pub struct Cfg<T: Configuration>(UnsafeCell<CfgInner<T>>, PhantomData<rc::Rc<T>>);

#[derive(Debug)]
struct CfgInner<T> {
    value: Arc<T>,
    storage: Arc<Storage>,
}

impl<T: Configuration> Cfg<T> {
    fn new(value: Arc<T>, storage: Arc<Storage>) -> Self {
        Self(UnsafeCell::new(CfgInner { value, storage }), PhantomData)
    }

    #[inline]
    /// Returns the unique identifier of the shared configuration.
    pub fn id(&self) -> usize {
        self.inner().storage.ctx.id()
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.inner().storage.ctx.tag()
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.inner().storage.ctx.service()
    }

    /// Returns a handle to the complete shared configuration.
    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.inner().storage.clone())
    }

    /// Returns another typed value from the same shared configuration.
    pub fn get_shared<U>(&self) -> Cfg<U>
    where
        U: Configuration,
    {
        self.shared().get::<U>()
    }

    fn inner(&self) -> &CfgInner<T> {
        unsafe { &*self.0.get() }
    }

    fn get_ref(&self) -> &T {
        &self.inner().value
    }

    #[allow(clippy::needless_pass_by_value)]
    /// Replaces the inner value.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that no references to the inner `T` value
    /// exist at the time this function is called.
    pub unsafe fn replace(&self, cfg: Cfg<T>) {
        unsafe {
            ptr::swap(self.0.get(), cfg.0.get());
        }
    }
}

impl<T: Configuration> Clone for Cfg<T> {
    #[inline]
    fn clone(&self) -> Self {
        let inner = self.inner();
        Self::new(inner.value.clone(), inner.storage.clone())
    }
}

impl<T: Configuration> From<T> for Cfg<T> {
    #[inline]
    fn from(cfg: T) -> Self {
        SharedCfg::new("--").add(cfg).build().get()
    }
}

impl<'a, T: Configuration> From<&'a T> for Cfg<T> {
    #[inline]
    fn from(cfg: &'a T) -> Self {
        cfg.ctx().get()
    }
}

impl<T: Configuration> ops::Deref for Cfg<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.get_ref()
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
    storage: Arc<Storage>,
    data: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Eq for SharedCfg {}

impl PartialEq for SharedCfg {
    fn eq(&self, other: &Self) -> bool {
        ptr::from_ref(self.0.as_ref()) == ptr::from_ref(other.0.as_ref())
    }
}

impl Hash for SharedCfg {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id().hash(state);
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
        self.0.ctx.id()
    }

    #[inline]
    /// Returns the configuration tag.
    pub fn tag(&self) -> &'static str {
        self.0.ctx.tag()
    }

    /// Returns the configured service name.
    pub fn service(&self) -> &'static str {
        self.0.ctx.service()
    }

    /// Returns the configuration value of type `T`.
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
        DEFAULT_CFG.with(Clone::clone)
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
        SharedCfgBuilder {
            storage: Storage::new(tag),
            data: HashMap::default(),
        }
    }

    #[must_use]
    /// Sets the service name.
    pub fn service(self, name: &'static str) -> Self {
        *self.storage.ctx.0.service.write().unwrap() = name;
        self
    }

    #[must_use]
    /// Inserts a typed value into this configuration.
    ///
    /// Replaces the existing value when the same type was already inserted.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(self.storage.ctx.clone());
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
        cfg.storage.data.set(cfg.data).unwrap();
        SharedCfg(cfg.storage)
    }
}

fn get<T>(st: &Arc<Storage>) -> Cfg<T>
where
    T: Configuration,
{
    let data = st.data.get().unwrap_or_else(|| {
        panic!(
            "{}: Cannot access shared config while building",
            st.ctx.tag()
        )
    });

    let tp = TypeId::of::<T>();
    let value = if let Some(arc) = data.get(&tp) {
        arc.clone().downcast::<T>().unwrap()
    } else {
        MAPPING.with(|store| {
            let key = (st.ctx.id(), tp);
            let cached = store.borrow().get(&key).cloned();
            if let Some(arc) = cached {
                arc.downcast::<T>().unwrap()
            } else {
                log::info!(
                    "{}: Configuration {:?} does not exist, using default",
                    st.ctx.tag(),
                    T::NAME
                );
                let mut val = T::default();
                val.set_ctx(st.ctx.clone());
                let arc = Arc::new(val);
                let previous = store.borrow_mut().insert(key, arc.clone());
                drop(previous);
                arc
            }
        })
    };
    Cfg::new(value, st.clone())
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Mutex, mpsc};

    use super::*;

    #[derive(Debug, Default)]
    struct ProbeCfg {
        ctx: CfgContext,
        capture: Option<Arc<Mutex<Option<CfgContext>>>>,
        drops: Option<Arc<AtomicUsize>>,
    }

    impl Configuration for ProbeCfg {
        const NAME: &str = "PROBE";

        fn ctx(&self) -> &CfgContext {
            &self.ctx
        }

        fn set_ctx(&mut self, ctx: CfgContext) {
            if let Some(capture) = &self.capture {
                *capture.lock().unwrap() = Some(ctx.clone());
            }
            self.ctx = ctx;
        }
    }

    impl Drop for ProbeCfg {
        fn drop(&mut self) {
            if let Some(drops) = &self.drops {
                drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn default_context_outlives_owner_thread() {
        let ctx = std::thread::spawn(CfgContext::default).join().unwrap();
        assert_eq!(ctx.tag(), "--");
        assert_eq!(ctx.service(), "");
        assert!(ctx.try_shared().is_none());
        assert!(ctx.try_get::<ProbeCfg>().is_none());
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.shared())).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| ctx.get::<ProbeCfg>())).is_err());
    }

    #[test]
    fn context_keeps_metadata_but_not_values_alive() {
        let drops = Arc::new(AtomicUsize::new(0));
        let shared = SharedCfg::new("TAG")
            .service("SERVICE")
            .add(ProbeCfg {
                ctx: CfgContext::default(),
                capture: None,
                drops: Some(drops.clone()),
            })
            .build();
        let id = shared.id();
        let cfg = shared.get::<ProbeCfg>();
        let clone = cfg.clone();
        let ctx = cfg.ctx().clone();
        drop(shared);
        drop(cfg);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(ctx.try_get::<ProbeCfg>().is_some());
        drop(clone);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.id(), id);
        assert_eq!(ctx.tag(), "TAG");
        assert_eq!(ctx.service(), "SERVICE");
        assert!(ctx.try_shared().is_none());
    }

    #[test]
    fn escaped_builder_context_observes_publication() {
        let capture = Arc::new(Mutex::new(None));
        let drops = Arc::new(AtomicUsize::new(0));
        let builder = SharedCfg::new("BUILDING").add(ProbeCfg {
            ctx: CfgContext::default(),
            capture: Some(capture.clone()),
            drops: Some(drops.clone()),
        });
        let ctx = capture.lock().unwrap().take().unwrap();
        let other = ctx.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let shared = other.shared();
            assert_eq!(other.service(), "BUILDING");
            assert!(catch_unwind(AssertUnwindSafe(|| other.try_get::<ProbeCfg>())).is_err());
            ready_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            assert_eq!(other.service(), "PUBLISHED");
            assert_eq!(other.try_get::<ProbeCfg>().unwrap().shared(), shared);
            shared
        });
        ready_rx.recv().unwrap();
        let shared = builder.service("PUBLISHED").build();
        resume_tx.send(()).unwrap();
        let other = thread.join().unwrap();
        assert_eq!(shared, other);
        drop(shared);
        drop(other);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.service(), "PUBLISHED");
        assert!(ctx.try_shared().is_none());
    }

    #[test]
    fn escaped_context_outlives_abandoned_builder() {
        let capture = Arc::new(Mutex::new(None));
        let drops = Arc::new(AtomicUsize::new(0));
        let builder = SharedCfg::new("ABANDONED").add(ProbeCfg {
            ctx: CfgContext::default(),
            capture: Some(capture.clone()),
            drops: Some(drops.clone()),
        });
        let ctx = capture.lock().unwrap().take().unwrap();
        drop(builder);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(ctx.tag(), "ABANDONED");
        assert_eq!(ctx.service(), "ABANDONED");
        assert!(ctx.try_get::<ProbeCfg>().is_none());
    }

    #[test]
    fn handle_ownership_does_not_depend_on_value_context() {
        #[derive(Debug, Default)]
        struct Unassociated(ProbeCfg);

        impl Configuration for Unassociated {
            const NAME: &str = "UNASSOCIATED";

            fn ctx(&self) -> &CfgContext {
                &self.0.ctx
            }

            fn set_ctx(&mut self, _: CfgContext) {}
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let shared = SharedCfg::new("OWNER")
            .add(Unassociated(ProbeCfg {
                ctx: CfgContext::default(),
                capture: None,
                drops: Some(drops.clone()),
            }))
            .build();
        let cfg = shared.get::<Unassociated>();
        let clone = cfg.clone();
        assert_eq!(cfg.ctx().tag(), "--");
        assert_eq!(cfg.tag(), "OWNER");
        assert_eq!(cfg.id(), shared.id());
        assert_eq!(cfg.shared(), shared);
        assert_eq!(cfg.get_shared::<Unassociated>().shared(), shared);
        assert!(ptr::eq(&raw const *cfg, &raw const *clone));
        drop(shared);
        drop(cfg);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(clone);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn caught_building_lookup_panic_preserves_owner() {
        #[derive(Debug, Default)]
        struct Recovering(CfgContext);

        impl Configuration for Recovering {
            const NAME: &str = "RECOVERING";

            fn ctx(&self) -> &CfgContext {
                &self.0
            }

            fn set_ctx(&mut self, ctx: CfgContext) {
                assert!(catch_unwind(AssertUnwindSafe(|| ctx.get::<Self>())).is_err());
                self.0 = ctx;
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let shared = SharedCfg::new("RECOVERED")
            .add(ProbeCfg {
                ctx: CfgContext::default(),
                capture: None,
                drops: Some(drops.clone()),
            })
            .add(Recovering::default())
            .service("READY")
            .build();
        assert_eq!(shared.get::<Recovering>().service(), "READY");
        drop(shared);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn default_lookup_panics_do_not_leak_storage() {
        #[derive(Debug)]
        struct PanicDefault;

        impl Default for PanicDefault {
            fn default() -> Self {
                panic!("default failed")
            }
        }

        impl Configuration for PanicDefault {
            const NAME: &str = "PANIC_DEFAULT";

            fn ctx(&self) -> &CfgContext {
                unreachable!()
            }

            fn set_ctx(&mut self, _: CfgContext) {
                unreachable!()
            }
        }

        #[derive(Debug, Default)]
        struct PanicContext(CfgContext);

        impl Configuration for PanicContext {
            const NAME: &str = "PANIC_CONTEXT";

            fn ctx(&self) -> &CfgContext {
                &self.0
            }

            fn set_ctx(&mut self, _: CfgContext) {
                panic!("context assignment failed")
            }
        }

        fn check<T: Configuration>() {
            let drops = Arc::new(AtomicUsize::new(0));
            let shared = SharedCfg::new("PANIC").add(ProbeCfg {
                ctx: CfgContext::default(),
                capture: None,
                drops: Some(drops.clone()),
            });
            let shared = shared.build();
            let storage = Arc::downgrade(&shared.0);
            assert!(catch_unwind(AssertUnwindSafe(|| shared.get::<T>())).is_err());
            drop(shared);
            assert!(storage.upgrade().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }

        check::<PanicDefault>();
        check::<PanicContext>();
    }

    #[test]
    #[allow(clippy::should_panic_without_expect)]
    #[should_panic]
    fn access_cfg_in_building_state() {
        #[derive(Debug)]
        struct TestCfg {
            config: CfgContext,
        }
        impl TestCfg {
            fn new() -> Self {
                Self {
                    config: CfgContext::default(),
                }
            }
        }
        impl Default for TestCfg {
            fn default() -> Self {
                panic!()
            }
        }
        impl Configuration for TestCfg {
            const NAME: &str = "TEST";
            fn ctx(&self) -> &CfgContext {
                &self.config
            }
            fn set_ctx(&mut self, ctx: CfgContext) {
                let _ = ctx.shared().get::<TestCfg>();
                self.config = ctx;
            }
        }
        let _ = TestCfg::new().ctx();
        let _ = SharedCfg::new("TEST").add(TestCfg::new());
    }

    #[test]
    fn shared_cfg() {
        #[derive(Default, Debug)]
        struct TestCfg {
            config: CfgContext,
        }
        impl Configuration for TestCfg {
            const NAME: &str = "TEST";
            fn ctx(&self) -> &CfgContext {
                &self.config
            }
            fn set_ctx(&mut self, ctx: CfgContext) {
                self.config = ctx;
            }
        }

        let cfg: SharedCfg = SharedCfg::new("TEST")
            .add(TestCfg::default())
            .service("SVC")
            .into();

        assert_eq!(cfg.tag(), "TEST");
        assert_eq!(cfg.service(), "SVC");
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST");
        assert_eq!(t.service(), "SVC");
        assert_eq!(t.shared(), cfg);
        let t: Cfg<TestCfg> = Cfg::default();
        assert_eq!(t.tag(), "--");
        assert_eq!(t.service(), "");
        assert_eq!(t.ctx().id(), t.id());

        let t: Cfg<TestCfg> = t.ctx().get();
        assert_eq!(t.tag(), "--");
        assert_eq!(t.ctx().id(), t.id());

        let cfg = SharedCfg::new("TEST2").build();
        let cfg = SharedCfg::from(&cfg);
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST2");
        assert_eq!(t.id(), cfg.id());
        drop(cfg);

        let cfg2 = t.shared();
        let t2 = cfg2.get::<TestCfg>();
        assert_eq!(t2.tag(), "TEST2");
        assert_eq!(t2.id(), cfg2.id());
        unsafe { t2.replace(SharedCfg::from(SharedCfg::new("TEST3")).get::<TestCfg>()) };

        let cfg2 = t2.shared();
        let t3 = cfg2.get::<TestCfg>();
        assert_eq!(t3.tag(), "TEST3");
        assert_eq!(t3.id(), cfg2.id());

        let t = SharedCfg::from(SharedCfg::new("TEST4").add(TestCfg::default())).get::<TestCfg>();
        let cfg = t.shared();
        assert_eq!(t.id(), cfg.id());
        let t2 = t.clone();
        assert_eq!(t2.id(), cfg.id());
        assert_eq!(t2.tag(), "TEST4");

        let t3 = t.ctx().get::<TestCfg>();
        let t3 = Cfg::from(t3.get_ref());
        let t3 = t3.get_shared::<TestCfg>();
        assert_eq!(t3.id(), cfg.id());
        assert_eq!(t3.tag(), "TEST4");

        let t1 = SharedCfg::from(t3);
        assert_eq!(t.id(), t1.id());
    }

    #[test]
    fn cfg_conversions() {
        use std::hash::BuildHasher;

        #[derive(Default, Debug)]
        struct TestCfg {
            config: CfgContext,
            val: usize,
        }
        impl Configuration for TestCfg {
            const NAME: &str = "TEST";
            fn ctx(&self) -> &CfgContext {
                &self.config
            }
            fn set_ctx(&mut self, ctx: CfgContext) {
                self.config = ctx;
            }
        }
        let new = |val| TestCfg {
            val,
            ..Default::default()
        };

        let t = Cfg::from(new(1));
        assert_eq!(t.val, 1);
        assert_eq!(t.tag(), "--");

        let shared = SharedCfg::from(new(2));
        assert_eq!(shared.get::<TestCfg>().val, 2);
        assert_eq!(Cfg::<TestCfg>::from(&shared).val, 2);
        assert_eq!(Cfg::<TestCfg>::from(shared.clone()).val, 2);

        let t: Cfg<TestCfg> = SharedCfg::new("T").add(new(3)).into();
        assert_eq!(t.val, 3);
        assert_eq!(t.tag(), "T");

        let hasher = foldhash::fast::FixedState::default();
        assert_eq!(hasher.hash_one(&shared), hasher.hash_one(shared.clone()));
        assert_eq!(hasher.hash_one(&shared), hasher.hash_one(shared.id()));
    }
}
