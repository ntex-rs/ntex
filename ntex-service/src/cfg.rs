//! Shared configuration for services
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::{any::Any, any::TypeId, fmt, ops, ptr};

type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

thread_local! {
    static DEFAULT_CFG: &'static Storage = Box::leak(Box::new(
        Storage { tag: "--".to_string(), building: false, data: HashMap::default()}));
}

#[derive(Debug)]
struct Storage {
    tag: String,
    building: bool,
    data: HashMap<TypeId, Box<dyn Any + Send + Sync>>,
}

pub trait Configuration: Send + Sync + fmt::Debug + 'static {
    const NAME: &'static str;

    /// Get default config item
    fn default() -> &'static Self;

    fn ctx(&self) -> &CfgContext;

    fn set_ctx(&mut self, ctx: CfgContext);
}

#[derive(Copy, Clone, Debug)]
pub struct CfgContext(&'static Storage);

impl CfgContext {
    pub fn tag(&self) -> &'static str {
        self.0.tag.as_ref()
    }

    pub fn shared(&self) -> SharedCfg {
        SharedCfg(self.0)
    }
}

impl Default for CfgContext {
    fn default() -> Self {
        CfgContext(DEFAULT_CFG.with(|cfg| *cfg))
    }
}

#[derive(Debug)]
pub struct Cfg<T: Configuration>(&'static T);

impl<T: Configuration> Cfg<T> {
    pub fn tag(&self) -> &'static str {
        self.0.ctx().tag()
    }

    pub fn shared(&self) -> SharedCfg {
        self.0.ctx().shared()
    }

    pub fn into_static(&self) -> &'static T {
        self.0
    }
}

impl<T: Configuration> Copy for Cfg<T> {}

impl<T: Configuration> Clone for Cfg<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Configuration> ops::Deref for Cfg<T> {
    type Target = T;

    fn deref(&self) -> &'static T {
        self.0
    }
}

impl<T: Configuration> Default for Cfg<T> {
    fn default() -> Self {
        Self(T::default())
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
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.tag.as_ref()
    }

    /// Get a reference to a previously inserted on configuration.
    ///
    /// Panics if shared config is building
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
        if self.0.building {
            panic!("Cannot access shared config while building");
        }
        self.0
            .data
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
            .map(Cfg)
            .unwrap_or_else(|| Cfg(T::default()))
    }
}

impl Default for SharedCfg {
    fn default() -> Self {
        Self(DEFAULT_CFG.with(|cfg| *cfg))
    }
}

impl SharedCfgBuilder {
    fn new(tag: String) -> SharedCfgBuilder {
        let storage = Box::into_raw(Box::new(Storage {
            tag,
            building: true,
            data: HashMap::default(),
        }));
        unsafe {
            SharedCfgBuilder {
                ctx: CfgContext(storage.as_ref().unwrap()),
                storage: Some(Box::from_raw(storage)),
            }
        }
    }

    /// Insert a type into this configuration.
    ///
    /// If a config of this type already existed, it will
    /// be replaced.
    pub fn add<T: Configuration>(mut self, mut val: T) -> Self {
        val.set_ctx(self.ctx);
        self.storage
            .as_mut()
            .unwrap()
            .data
            .insert(TypeId::of::<T>(), Box::new(val))
            .and_then(|item| item.downcast::<T>().map(|boxed| *boxed).ok());
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
        impl Configuration for TestCfg {
            const NAME: &str = "TEST";
            fn default() -> &'static Self {
                panic!()
            }
            fn ctx(&self) -> &CfgContext {
                &self.config
            }
            fn set_ctx(&mut self, ctx: CfgContext) {
                self.config = ctx;
                let _ = ctx.shared().get::<TestCfg>();
            }
        }

        SharedCfg::new("TEST").add(TestCfg::new());
    }

    #[test]
    fn shared_cfg() {
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
        impl Configuration for TestCfg {
            const NAME: &str = "TEST";
            fn default() -> &'static Self {
                thread_local! {
                    static DEFAULT_CFG: &'static TestCfg = Box::leak(Box::new(TestCfg::new()));
                }
                DEFAULT_CFG.with(|cfg| *cfg)
            }
            fn ctx(&self) -> &CfgContext {
                &self.config
            }
            fn set_ctx(&mut self, ctx: CfgContext) {
                self.config = ctx;
            }
        }

        let cfg: SharedCfg = SharedCfg::new("TEST").add(TestCfg::new()).into();

        assert_eq!(cfg.tag(), "TEST");
        let t = cfg.get::<TestCfg>();
        assert_eq!(t.tag(), "TEST");
        assert_eq!(t.shared(), cfg);
        let t: Cfg<TestCfg> = Default::default();
        assert_eq!(t.tag(), "--");
    }
}
