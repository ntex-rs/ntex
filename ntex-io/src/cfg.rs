use std::sync::atomic::{AtomicUsize, Ordering};
use std::{any::Any, any::TypeId, cell::UnsafeCell, mem, ops};

use ntex_bytes::{BytesVec, buf::BufMut};
use ntex_util::{HashMap, time::Seconds};

const DEFAULT_CACHE_SIZE: usize = 128;
static IDX: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static CACHE: LocalCache = LocalCache::new();
    static DEFAULT_MAP: &'static Storage = Box::leak(Box::new(("IO".to_string(), HashMap::default())));
}

type Storage = (String, HashMap<TypeId, Box<dyn Any + Send + Sync>>);

#[derive(Debug)]
pub struct Cfg<T: 'static>(&'static T);

impl<T: 'static> Copy for Cfg<T> {}

impl<T: 'static> Clone for Cfg<T> {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl<T: 'static> ops::Deref for Cfg<T> {
    type Target = T;

    fn deref(&self) -> &'static T {
        &self.0
    }
}

impl<T: 'static> Cfg<T> {
    pub fn into_static(&self) -> &'static T {
        self.0
    }
}

#[derive(Copy, Clone, Debug)]
/// Shared configuration
pub struct SharedConfig(&'static Storage);

#[derive(Debug)]
pub struct SharedConfigBuilder(Storage);

impl SharedConfig {
    /// Construct new configuration
    pub fn new<T: AsRef<str>>(tag: T) -> SharedConfig {
        Self::build(tag).finish()
    }

    /// Construct new configuration
    pub fn build<T: AsRef<str>>(tag: T) -> SharedConfigBuilder {
        SharedConfigBuilder((tag.as_ref().to_string(), HashMap::default()))
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &'static str {
        self.0.0.as_ref()
    }

    /// Get a reference to a previously inserted on configuration.
    pub fn get<T>(&self) -> Cfg<T>
    where
        &'static T: Default,
    {
        self.0
            .1
            .get(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
            .map(Cfg)
            .unwrap_or_else(|| Cfg(<&'static T>::default()))
    }
}

impl Default for SharedConfig {
    fn default() -> Self {
        Self(DEFAULT_MAP.with(|cfg| *cfg))
    }
}

impl SharedConfigBuilder {
    /// Insert a type into this configuration.
    ///
    /// If a config of this type already existed, it will
    /// be replaced.
    pub fn add<T>(&mut self, val: T) -> &mut Self
    where
        T: Send + Sync + 'static,
        &'static T: Default,
    {
        self.0
            .1
            .insert(TypeId::of::<T>(), Box::new(val))
            .and_then(|item| item.downcast::<T>().map(|boxed| *boxed).ok());
        self
    }

    /// Build static ref
    pub fn finish(&mut self) -> SharedConfig {
        if !self.0.1.contains_key(&TypeId::of::<IoConfig>()) {
            self.add(IoConfig::new());
        }

        let map = mem::take(&mut self.0);
        let cfg: *mut IoConfig = self
            .0
            .1
            .get_mut(&TypeId::of::<IoConfig>())
            .and_then(|boxed| boxed.downcast_mut())
            .unwrap();

        let map_ref = Box::leak(Box::new(map));

        unsafe {
            (*cfg).config = map_ref;
        }
        SharedConfig(map_ref)
    }
}

#[derive(Clone, Debug)]
/// Base io configuration
pub struct IoConfig {
    keepalive_timeout: Seconds,
    disconnect_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,

    // io read/write cache and params
    read_buf: BufConfig,
    write_buf: BufConfig,

    // shared config
    config: &'static Storage,
}

#[derive(Copy, Clone, Debug)]
pub struct FrameReadRate {
    pub timeout: Seconds,
    pub max_timeout: Seconds,
    pub rate: u32,
}

#[derive(Copy, Clone, Debug)]
pub struct BufConfig {
    pub high: usize,
    pub low: usize,
    pub half: usize,
    idx: usize,
    cache_size: usize,
}

impl Default for &'static IoConfig {
    fn default() -> &'static IoConfig {
        thread_local! {
            static DEFAULT_CFG: &'static IoConfig =
                Box::leak(Box::new(IoConfig::new()));
        }
        DEFAULT_CFG.with(|cfg| *cfg)
    }
}

impl IoConfig {
    #[inline]
    /// Create new config object
    pub fn new() -> IoConfig {
        let idx1 = IDX.fetch_add(1, Ordering::SeqCst);
        let idx2 = IDX.fetch_add(1, Ordering::SeqCst);
        IoConfig {
            keepalive_timeout: Seconds(0),
            disconnect_timeout: Seconds(1),
            frame_read_rate: None,

            read_buf: BufConfig {
                high: 16 * 1024,
                low: 1024,
                half: 8 * 1024,
                idx: idx1,
                cache_size: DEFAULT_CACHE_SIZE,
            },
            write_buf: BufConfig {
                high: 16 * 1024,
                low: 1024,
                half: 8 * 1024,
                idx: idx2,
                cache_size: DEFAULT_CACHE_SIZE,
            },
            config: DEFAULT_MAP.with(|cfg| *cfg),
        }
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &str {
        self.config.0.as_ref()
    }

    #[inline]
    /// Get keep-alive timeout
    pub fn keepalive_timeout(&self) -> Seconds {
        self.keepalive_timeout
    }

    #[inline]
    /// Get disconnect timeout
    pub fn disconnect_timeout(&self) -> Seconds {
        self.disconnect_timeout
    }

    #[inline]
    /// Get frame read params
    pub fn frame_read_rate(&self) -> Option<&FrameReadRate> {
        self.frame_read_rate.as_ref()
    }

    #[inline]
    /// Get read buffer parameters
    pub fn read_buf(&self) -> &BufConfig {
        &self.read_buf
    }

    #[inline]
    /// Get write buffer parameters
    pub fn write_buf(&self) -> &BufConfig {
        &self.write_buf
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn set_keepalive_timeout(mut self, timeout: Seconds) -> Self {
        self.keepalive_timeout = timeout;
        self
    }

    /// Set connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn set_disconnect_timeout(mut self, timeout: Seconds) -> Self {
        self.disconnect_timeout = timeout;
        self
    }

    /// Set read rate parameters for single frame.
    ///
    /// Set read timeout, max timeout and rate for reading payload. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default frame read rate is disabled.
    pub fn set_frame_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> Self {
        self.frame_read_rate = Some(FrameReadRate {
            timeout,
            max_timeout,
            rate,
        });
        self
    }

    /// Set read buffer parameters.
    ///
    /// By default high watermark is set to 16Kb, low watermark 1kb.
    pub fn set_read_buf(
        mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> Self {
        self.read_buf.cache_size = cache_size;
        self.read_buf.high = high_watermark;
        self.read_buf.low = low_watermark;
        self.read_buf.half = high_watermark >> 1;
        self
    }

    /// Set write buffer parameters.
    ///
    /// By default high watermark is set to 16Kb, low watermark 1kb.
    pub fn set_write_buf(
        mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> Self {
        self.write_buf.cache_size = cache_size;
        self.write_buf.high = high_watermark;
        self.write_buf.low = low_watermark;
        self.write_buf.half = high_watermark >> 1;
        self
    }
}

impl BufConfig {
    #[inline]
    /// Get buffer
    pub fn get(&self) -> BytesVec {
        if let Some(mut buf) = CACHE.with(|c| c.with(self.idx, |c: &mut Vec<_>| c.pop())) {
            buf.clear();
            buf
        } else {
            BytesVec::with_capacity(self.high)
        }
    }

    /// Get buffer with capacity
    pub fn buf_with_capacity(&self, cap: usize) -> BytesVec {
        BytesVec::with_capacity(cap)
    }

    #[inline]
    /// Resize buffer
    pub fn resize(&self, buf: &mut BytesVec) {
        let remaining = buf.remaining_mut();
        if remaining < self.low {
            buf.reserve(self.high - remaining);
        }
    }

    #[inline]
    /// Release buffer, buf must be allocated from this pool
    pub fn release(&self, buf: BytesVec) {
        let cap = buf.capacity();
        if cap > self.low && cap <= self.high {
            CACHE.with(|c| {
                c.with(self.idx, |v: &mut Vec<_>| {
                    if v.len() < self.cache_size {
                        v.push(buf);
                    }
                })
            })
        }
    }
}

struct LocalCache {
    cache: UnsafeCell<Vec<Vec<BytesVec>>>,
}

impl LocalCache {
    fn new() -> Self {
        Self {
            cache: UnsafeCell::new(Vec::with_capacity(16)),
        }
    }

    fn with<F, R>(&self, idx: usize, f: F) -> R
    where
        F: FnOnce(&mut Vec<BytesVec>) -> R,
    {
        let cache = unsafe { &mut *self.cache.get() };

        while cache.len() <= idx {
            cache.push(Vec::new())
        }
        f(&mut cache[idx])
    }
}
