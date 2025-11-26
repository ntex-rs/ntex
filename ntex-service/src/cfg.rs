//! Shared configuration for services
#![allow(clippy::should_implement_trait, clippy::new_ret_no_self)]
use std::{any::Any, any::TypeId, fmt, ops};

type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

thread_local! {
    static DEFAULT_CFG: &'static Storage = Box::leak(Box::new(
        Storage { tag: "--".to_string(), data: HashMap::default()}));
}

#[derive(Debug)]
struct Storage {
    tag: String,
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
    pub fn get<T>(&self) -> Cfg<T>
    where
        T: Configuration,
    {
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
        if let Some(st) = self.storage.take() {
            let _ = Box::leak(st);
        }
    }
}

impl From<SharedCfgBuilder> for SharedCfg {
    fn from(mut cfg: SharedCfgBuilder) -> SharedCfg {
        SharedCfg(Box::leak(cfg.storage.take().unwrap()))
    }
}
