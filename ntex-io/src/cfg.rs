use std::cell::UnsafeCell;

use ntex_bytes::{BytesVec, buf::BufMut};
use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{time::Millis, time::Seconds};

const DEFAULT_CACHE_SIZE: usize = 128;
const DEFAULT_HIGH: usize = 16 * 1024 - 24;
const DEFAULT_LOW: usize = 512 + 24;
const DEFAULT_HALF: usize = (16 * 1024 - 24) / 2;

thread_local! {
    static CACHE: LocalCache = LocalCache::new();
}

#[derive(Clone, Debug)]
/// Base io configuration
pub struct IoConfig {
    connect_timeout: Millis,
    keepalive_timeout: Seconds,
    disconnect_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,

    // io read/write cache and params
    read_buf: BufConfig,
    write_buf: BufConfig,

    // shared config
    pub(crate) config: CfgContext,
}

impl Default for IoConfig {
    fn default() -> Self {
        IoConfig::new()
    }
}

impl Configuration for IoConfig {
    const NAME: &str = "IO Configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
        self.read_buf.idx = ctx.id();
        self.write_buf.idx = ctx.id();
    }
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
    first: bool,
    cache_size: usize,
}

impl IoConfig {
    #[inline]
    #[allow(clippy::new_without_default)]
    /// Create new config object
    pub fn new() -> IoConfig {
        let config = CfgContext::default();

        IoConfig {
            config,
            connect_timeout: Millis::ZERO,
            keepalive_timeout: Seconds(0),
            disconnect_timeout: Seconds(1),
            frame_read_rate: None,

            read_buf: BufConfig {
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
                idx: config.id(),
                first: true,
                cache_size: DEFAULT_CACHE_SIZE,
            },
            write_buf: BufConfig {
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
                idx: config.id(),
                first: false,
                cache_size: DEFAULT_CACHE_SIZE,
            },
        }
    }

    #[inline]
    /// Get tag
    pub fn tag(&self) -> &str {
        self.config.tag()
    }

    #[inline]
    /// Get connect timeout
    pub fn connect_timeout(&self) -> Millis {
        self.connect_timeout
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

    /// Set connect timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default connect timeout is disabled.
    pub fn set_connect_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.connect_timeout = timeout.into();
        self
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is disabled.
    pub fn set_keepalive_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.keepalive_timeout = timeout.into();
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
    pub fn set_disconnect_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.disconnect_timeout = timeout.into();
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
        if let Some(mut buf) =
            CACHE.with(|c| c.with(self.idx, self.first, |c: &mut Vec<_>| c.pop()))
        {
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
                c.with(self.idx, self.first, |v: &mut Vec<_>| {
                    if v.len() < self.cache_size {
                        v.push(buf);
                    }
                })
            })
        }
    }
}

struct LocalCache {
    cache: UnsafeCell<Vec<(Vec<BytesVec>, Vec<BytesVec>)>>,
}

impl LocalCache {
    fn new() -> Self {
        Self {
            cache: UnsafeCell::new(Vec::with_capacity(16)),
        }
    }

    fn with<F, R>(&self, idx: usize, first: bool, f: F) -> R
    where
        F: FnOnce(&mut Vec<BytesVec>) -> R,
    {
        let cache = unsafe { &mut *self.cache.get() };

        while cache.len() <= idx {
            cache.push((Vec::new(), Vec::new()))
        }
        if first {
            f(&mut cache[idx].0)
        } else {
            f(&mut cache[idx].1)
        }
    }
}
