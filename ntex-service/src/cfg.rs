//! Shared configuration for services
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{any::Any, any::TypeId, cell::RefCell, fmt, ops, ptr};

type Key = (usize, TypeId);
type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

thread_local! {
    static DEFAULT_CFG: &'static Storage = Box::leak(Box::new(
        Storage::new("--".to_string(), false)));
    static MAPPING: RefCell<HashMap<Key, &'static dyn Any>> = {
        RefCell::new(HashMap::default())
    };
}
static IDX: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct Storage {
    id: usize,
    tag: String,
    building: bool,
    data: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

impl Storage {
    fn new(tag: String, building: bool) -> Self {
        let id = IDX.fetch_add(1, Ordering::SeqCst);
        Storage {
            id,
            tag,
            building,
            data: HashMap::default(),
        }
    }
}

pub trait Configuration: Default + Send + Sync + fmt::Debug + 'static {
    const NAME: &'static str;

    fn ctx(&self) -> &CfgContext;

    fn set_ctx(&mut self, ctx: CfgContext);
}

#[derive(Copy, Clone, Debug)]
pub struct CfgContext(&'static Storage);

impl CfgContext {
    #[inline]
    pub fn id(&self) -> usize {
        self.0.id
    }

    #[inline]
    pub fn tag(&self) -> &'static str {
        self.0.tag.as_ref()
    }

    #[inline]
    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.0)
    }
}

impl Default for CfgContext {
    #[inline]
    fn default() -> Self {
        CfgContext(DEFAULT_CFG.with(|cfg| *cfg))
    }
}

#[derive(Debug)]
pub struct Cfg<T: Configuration>(&'static T);

impl<T: Configuration> Cfg<T> {
    #[inline]
    pub fn id(&self) -> usize {
        self.0.ctx().0.id
    }

    #[inline]
    pub fn tag(&self) -> &'static str {
        self.0.ctx().tag()
    }

    #[inline]
    pub fn shared(&self) -> SharedCfg {
        self.0.ctx().shared()
    }

    #[inline]
    pub fn into_static(&self) -> &'static T {
        self.0
    }
}

impl<T: Configuration> Copy for Cfg<T> {}

impl<T: Configuration> Clone for Cfg<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Configuration> ops::Deref for Cfg<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &'static T {
        self.0
    }
}

impl<T: Configuration> Default for Cfg<T> {
    #[inline]
    fn default() -> Self {
        CfgContext::default().shared().get()
    }
}

#[derive(Copy, Clone, Debug)]
/// Shared configuration
pub struct SharedCfg(&'static Storage);

#[derive(Debug)]
pub struct SharedCfgBuilder {
    ctx: CfgContext,
    storage: Option<Box<Storage>>,
}

impl Eq for SharedCfg {}

impl PartialEq for SharedCfg {
    fn eq(&self, other: &Self) -> bool {
        ptr::from_ref(self.0) == ptr::from_ref(other.0)
    }
}

impl SharedCfg {
    /// Construct new configuration
    pub fn new<T: AsRef<str>>(tag: T) -> SharedCfgBuilder {
        SharedCfgBuilder::new(tag.as_ref().to_string())
    }

    #[inline]
    /// Get unique shared cfg id
    pub fn id(&self) -> usize {
        self.0.id
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.tag.as_ref()
    }

    /// Get a reference to a previously inserted on configuration.
    ///
    /// # Panics
    /// if shared config is in building stage
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        assert!(
            !self.0.building,
            "{}: Cannot access shared config while building",
            self.tag()
        );

        let tp = TypeId::of::<T>();
        self.0
            .data
            .get(&tp)
            .and_then(|boxed| boxed.downcast_ref())
            .map_or_else(
                || {
                    MAPPING.with(|store| {
                        let key = (self.0.id, tp);
                        if let Some(boxed) = store.borrow().get(&key) {
                            Cfg(boxed.downcast_ref().unwrap())
                        } else {
                            log::info!(
                                "{}: Configuration {:?} does not exist, using default",
                                self.tag(),
                                T::NAME
                            );
                            let mut val = T::default();
                            val.set_ctx(CfgContext(self.0));
                            store.borrow_mut().insert(key, Box::leak(Box::new(val)));
                            Cfg(store.borrow().get(&key).unwrap().downcast_ref().unwrap())
                        }
                    })
                },
                Cfg,
            )
    }
}

impl Default for SharedCfg {
    #[inline]
    fn default() -> Self {
        Self(DEFAULT_CFG.with(|cfg| *cfg))
    }
}

impl SharedCfgBuilder {
    fn new(tag: String) -> SharedCfgBuilder {
        let storage = Box::into_raw(Box::new(Storage::new(tag, true)));
        unsafe {
            SharedCfgBuilder {
                ctx: CfgContext(storage.as_ref().unwrap()),
                storage: Some(Box::from_raw(storage)),
            }
        }
    }

    #[must_use]
    /// Insert a type into this configuration.
    ///
    /// If a config of this type already existed, it will
    /// be replaced.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(self.ctx);
        if let Some(st) = self.storage.as_mut() {
            st.data.insert(TypeId::of::<T>(), Box::new(val));
        }
        self
    }
}

impl Drop for SharedCfgBuilder {
    fn drop(&mut self) {
        if let Some(mut st) = self.storage.take() {
            st.building = false;
            let _ = Box::leak(st);
        }
    }
}

impl From<SharedCfgBuilder> for SharedCfg {
    fn from(mut cfg: SharedCfgBuilder) -> SharedCfg {
        let mut st = cfg.storage.take().unwrap();
        st.building = false;
        SharedCfg(Box::leak(st))
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
                self.config = ctx;
                let _ = ctx.shared().get::<TestCfg>();
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
        assert_eq!(t.ctx().id(), t.ctx().clone().id());

        let cfg: SharedCfg = SharedCfg::new("TEST2").into();
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST2");
        assert_eq!(t.id(), cfg.id());
    }
}
