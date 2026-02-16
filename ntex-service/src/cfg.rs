//! Shared configuration for services
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::any::{Any, TypeId};
use std::cell::{RefCell, UnsafeCell};
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{fmt, hash::Hash, hash::Hasher, marker::PhantomData, mem, ops, ptr, rc};

type Key = (usize, TypeId);
type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
type HashSet<V> = std::collections::HashSet<V, foldhash::fast::RandomState>;

thread_local! {
    static DEFAULT_CFG: Arc<Storage> = {
        let mut st = Arc::new(Storage::new("--", false));
        let ctx = CfgContext(Arc::as_ptr(&st));
        Arc::get_mut(&mut st).unwrap().ctx = ctx;
        st
    };
    static MAPPING: RefCell<HashMap<Key, Arc<dyn Any + Send + Sync>>> = {
        RefCell::new(HashMap::default())
    };
    static REFS: RefCell<HashSet<SharedCfg>> = RefCell::new(HashSet::default());
}
static IDX: AtomicUsize = AtomicUsize::new(0);

pub trait Configuration: Default + Send + Sync + fmt::Debug + 'static {
    const NAME: &'static str;

    fn ctx(&self) -> &CfgContext;

    fn set_ctx(&mut self, ctx: CfgContext);
}

#[derive(Debug)]
struct Storage {
    id: usize,
    tag: &'static str,
    ctx: CfgContext,
    building: bool,
    data: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Storage {
    fn new(tag: &'static str, building: bool) -> Self {
        let id = IDX.fetch_add(1, Ordering::SeqCst);
        Storage {
            id,
            tag,
            building,
            data: HashMap::default(),
            ctx: CfgContext(ptr::null()),
        }
    }
}

#[derive(Debug)]
pub struct CfgContext(*const Storage);

unsafe impl Send for CfgContext {}
unsafe impl Sync for CfgContext {}

impl CfgContext {
    #[inline]
    /// Unique id of the context.
    pub fn id(&self) -> usize {
        self.get_ref().id
    }

    #[inline]
    /// Context tag
    pub fn tag(&self) -> &'static str {
        self.get_ref().tag
    }

    #[inline]
    /// Get a reference to a previously inserted on configuration.
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        get(self.get_ref())
    }

    #[inline]
    /// Get a shared configuration.
    pub fn shared(&self) -> SharedCfg {
        let inner: Arc<Storage> = unsafe { Arc::from_raw(self.0) };
        let shared = SharedCfg(inner.clone());
        mem::forget(inner);
        shared
    }

    fn get_ref(&self) -> &Storage {
        unsafe { self.0.as_ref().unwrap() }
    }
}

impl Default for CfgContext {
    #[inline]
    fn default() -> Self {
        CfgContext(DEFAULT_CFG.with(Arc::as_ptr))
    }
}

#[derive(Debug)]
pub struct Cfg<T: Configuration>(UnsafeCell<*const T>, PhantomData<rc::Rc<T>>);

impl<T: Configuration> Cfg<T> {
    #[inline]
    /// Unique id of the configuration.
    pub fn id(&self) -> usize {
        self.get_ref().ctx().id()
    }

    #[inline]
    /// Context tag
    pub fn tag(&self) -> &'static str {
        self.get_ref().ctx().tag()
    }

    #[inline]
    /// Get a shared configuration.
    pub fn shared(&self) -> SharedCfg {
        self.get_ref().ctx().shared()
    }

    fn get_ref(&self) -> &T {
        unsafe { (*self.0.get()).as_ref().unwrap() }
    }

    /// Replaces the inner value.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that no references to the inner `T` value
    /// exist at the time this function is called.
    pub unsafe fn replace(&self, cfg: &Cfg<T>) {
        unsafe {
            ptr::swap(self.0.get(), cfg.0.get());
        }
    }
}

impl<T: Configuration> Drop for Cfg<T> {
    fn drop(&mut self) {
        unsafe { drop(Arc::<T>::from_raw(*self.0.get())) }
    }
}

impl<T: Configuration> Clone for Cfg<T> {
    #[inline]
    fn clone(&self) -> Self {
        let cloned = unsafe {
            let inner = Arc::<T>::from_raw(*self.0.get());
            let cloned = inner.clone();
            mem::forget(inner);
            Arc::into_raw(cloned).cast_mut()
        };
        Cfg(UnsafeCell::new(cloned), PhantomData)
    }
}

impl<'a, T: Configuration> From<&'a T> for Cfg<T> {
    #[inline]
    fn from(cfg: &'a T) -> Self {
        get(cfg.ctx().get_ref())
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
        CfgContext::default().shared().get()
    }
}

#[derive(Clone, Debug)]
/// Shared configuration
pub struct SharedCfg(Arc<Storage>);

#[derive(Debug)]
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
        ptr::from_ref(self.0.as_ref()).hash(state);
    }
}

impl SharedCfg {
    /// Construct new configuration
    pub fn new(tag: &'static str) -> SharedCfgBuilder {
        SharedCfgBuilder::new(tag)
    }

    #[inline]
    /// Get unique shared cfg id
    pub fn id(&self) -> usize {
        self.0.id
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.tag
    }

    /// Get a reference to a previously inserted on configuration.
    ///
    /// # Panics
    /// if shared config is in building stage
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        get(self.0.as_ref())
    }
}

impl Default for SharedCfg {
    #[inline]
    fn default() -> Self {
        Self(DEFAULT_CFG.with(Clone::clone))
    }
}

impl<T: Configuration> From<SharedCfg> for Cfg<T> {
    #[inline]
    fn from(cfg: SharedCfg) -> Self {
        cfg.get()
    }
}

impl SharedCfgBuilder {
    fn new(tag: &'static str) -> SharedCfgBuilder {
        let mut storage = Arc::new(Storage::new(tag, true));
        let ctx = CfgContext(Arc::as_ptr(&storage));
        Arc::get_mut(&mut storage).unwrap().ctx = CfgContext(ctx.0);

        SharedCfgBuilder { ctx, storage }
    }

    #[must_use]
    #[allow(clippy::missing_panics_doc)]
    /// Insert a type into this configuration.
    ///
    /// If a config of this type already existed, it will
    /// be replaced.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(CfgContext(self.ctx.0));
        Arc::get_mut(&mut self.storage)
            .unwrap()
            .data
            .insert(TypeId::of::<T>(), Arc::new(val));
        self
    }
}

impl From<SharedCfgBuilder> for SharedCfg {
    fn from(mut cfg: SharedCfgBuilder) -> SharedCfg {
        let st = Arc::get_mut(&mut cfg.storage).unwrap();
        st.building = false;
        SharedCfg(cfg.storage)
    }
}

fn get<T>(st: &Storage) -> Cfg<T>
where
    T: Configuration,
{
    assert!(
        !st.building,
        "{}: Cannot access shared config while building",
        st.tag
    );

    let tp = TypeId::of::<T>();
    if let Some(arc) = st.data.get(&tp) {
        Cfg(
            UnsafeCell::new(Arc::into_raw(arc.clone().downcast::<T>().unwrap())),
            PhantomData,
        )
    } else {
        MAPPING.with(|store| {
            let key = (st.id, tp);
            if let Some(arc) = store.borrow().get(&key) {
                Cfg(
                    UnsafeCell::new(Arc::into_raw(arc.clone().downcast::<T>().unwrap())),
                    PhantomData,
                )
            } else {
                log::info!(
                    "{}: Configuration {:?} does not exist, using default",
                    st.tag,
                    T::NAME
                );
                // store Storage ref, otherwise it could be deallocated
                let ctx = CfgContext(st.ctx.0);
                REFS.with(|refs| refs.borrow_mut().insert(ctx.shared()));

                let mut val = T::default();
                val.set_ctx(ctx);
                let val = Arc::new(val);
                store.borrow_mut().insert(key, val.clone());
                Cfg(UnsafeCell::new(Arc::into_raw(val)), PhantomData)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let cfg: SharedCfg = SharedCfg::new("TEST").add(TestCfg::default()).into();

        assert_eq!(cfg.tag(), "TEST");
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST");
        assert_eq!(t.shared(), cfg);
        let t: Cfg<TestCfg> = Cfg::default();
        assert_eq!(t.tag(), "--");
        assert_eq!(t.ctx().id(), t.id());

        let t: Cfg<TestCfg> = t.ctx().get();
        assert_eq!(t.tag(), "--");
        assert_eq!(t.ctx().id(), t.id());

        let cfg: SharedCfg = SharedCfg::new("TEST2").into();
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST2");
        assert_eq!(t.id(), cfg.id());
    }
}
