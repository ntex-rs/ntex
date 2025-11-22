use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use ntex_bytes::{buf::BufMut, BytesVec};
use ntex_util::time::Seconds;

const DEFAULT_TAG: &str = "IO";
const DEFAULT_CACHE_SIZE: usize = 128;
static IDX: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static DEFAULT_CFG: IoConfig = {
        IoConfig::new(DEFAULT_TAG)
    };
    static CACHE: LocalCache = {
        LocalCache::new()
    }
}

#[derive(Copy, Clone, Debug)]
/// Shared configuration
pub struct IoConfig(&'static IoConfigInner);

#[derive(Debug)]
/// Construct shared configuration
pub struct IoConfigBuilder(IoConfigInner);

#[derive(Copy, Clone, Debug)]
/// Shared configuration
struct IoConfigInner {
    tag: &'static str,
    keepalive_timeout: Seconds,
    disconnect_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,

    // io read/write cache and params
    read_buf: BufConfig,
    write_buf: BufConfig,
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

impl Default for IoConfig {
    /// Get default config
    fn default() -> IoConfig {
        DEFAULT_CFG.with(|cfg| *cfg)
    }
}

impl IoConfig {
    #[inline]
    /// Create new config object
    pub fn new(tag: &'static str) -> IoConfig {
        IoConfig::build(tag).finish()
    }

    /// Construct new configuration
    pub fn build(tag: &'static str) -> IoConfigBuilder {
        let idx1 = IDX.fetch_add(1, Ordering::SeqCst);
        let idx2 = IDX.fetch_add(1, Ordering::SeqCst);
        IoConfigBuilder(IoConfigInner {
            tag,
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
        })
    }

    #[inline]
    /// Get keep-alive timeout
    pub fn tag(&self) -> &'static str {
        self.0.tag
    }

    #[inline]
    /// Get keep-alive timeout
    pub fn keepalive_timeout(&self) -> Seconds {
        self.0.keepalive_timeout
    }

    #[inline]
    /// Get disconnect timeout
    pub fn disconnect_timeout(&self) -> Seconds {
        self.0.disconnect_timeout
    }

    #[inline]
    /// Get frame read params
    pub fn frame_read_rate(&self) -> Option<&'static FrameReadRate> {
        self.0.frame_read_rate.as_ref()
    }

    #[inline]
    /// Get read buffer parameters
    pub fn read_buf(&self) -> &'static BufConfig {
        &self.0.read_buf
    }

    #[inline]
    /// Get write buffer parameters
    pub fn write_buf(&self) -> &'static BufConfig {
        &self.0.write_buf
    }
}

impl IoConfigBuilder {
    /// Set tag
    pub fn set_tag(&mut self, tag: &'static str) -> &mut Self {
        self.0.tag = tag;
        self
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn set_keepalive_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.0.keepalive_timeout = timeout;
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
    pub fn set_disconnect_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.0.disconnect_timeout = timeout;
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
        &mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> &mut Self {
        self.0.frame_read_rate = Some(FrameReadRate {
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
        &mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> &mut Self {
        self.0.read_buf.cache_size = cache_size;
        self.0.read_buf.high = high_watermark;
        self.0.read_buf.low = low_watermark;
        self.0.read_buf.half = high_watermark >> 1;
        self
    }

    /// Set write buffer parameters.
    ///
    /// By default high watermark is set to 16Kb, low watermark 1kb.
    pub fn set_write_buf(
        &mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> &mut Self {
        self.0.write_buf.cache_size = cache_size;
        self.0.write_buf.high = high_watermark;
        self.0.write_buf.low = low_watermark;
        self.0.write_buf.half = high_watermark >> 1;
        self
    }

    /// Build static ref
    pub fn finish(&self) -> IoConfig {
        IoConfig(Box::leak(Box::new(self.0)))
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
