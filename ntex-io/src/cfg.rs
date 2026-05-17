use std::cell::UnsafeCell;

use ntex_bytes::{BytePageSize, BytesMut, buf::BufMut};
use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{time::Millis, time::Seconds};

const DEFAULT_CACHE_SIZE: usize = 128;
const DEFAULT_HIGH: usize = 16 * 1024 - 24;
const DEFAULT_LOW: usize = 512 + 24;
const DEFAULT_HALF: usize = (16 * 1024 - 24) / 2;

thread_local! {
    static CACHE: LocalCache = LocalCache::new();
}

#[derive(Debug)]
/// Base io configuration
pub struct IoConfig {
    connect_timeout: Millis,
    keepalive_timeout: Seconds,
    disconnect_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,

    // io read/write cache and params
    read_buf: BufConfig,
    write_buf: BufConfig,
    write_page_size: BytePageSize,
    write_buf_threshold: usize,

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
        self.read_buf.idx = ctx.id();
        self.write_buf.idx = ctx.id();
        self.config = ctx;
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
    #[must_use]
    /// Create new config object
    pub fn new() -> IoConfig {
        let config = CfgContext::default();
        let idx = config.id();

        IoConfig {
            config,
            connect_timeout: Millis::ZERO,
            keepalive_timeout: Seconds(0),
            disconnect_timeout: Seconds(1),
            frame_read_rate: None,

            read_buf: BufConfig {
                idx,
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
                first: true,
                cache_size: DEFAULT_CACHE_SIZE,
            },
            write_buf: BufConfig {
                idx,
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
                first: false,
                cache_size: DEFAULT_CACHE_SIZE,
            },
            write_page_size: BytePageSize::Size16,
            write_buf_threshold: BytePageSize::Size16.half_capacity(),
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

    #[inline]
    /// Get write page size
    pub fn write_page_size(&self) -> BytePageSize {
        self.write_page_size
    }

    #[inline]
    /// The write buffer threshold that triggers earlier sending.
    pub fn write_buf_threshold(&self) -> usize {
        self.write_buf_threshold
    }

    /// Set connect timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default connect timeout is disabled.
    #[must_use]
    pub fn set_connect_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.connect_timeout = timeout.into();
        self
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is disabled.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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

    /// Set write buffer page size.
    ///
    /// By default page size is set to 16kb.
    #[must_use]
    pub fn set_write_page_size(mut self, size: BytePageSize) -> Self {
        self.write_page_size = size;
        self
    }

    /// Sets the write buffer threshold.
    ///
    /// The app encodes data in response to incoming data,
    /// continuing to fill the write buffer until all data
    /// has been processed. Only then can the runtime wake
    /// the write task to send the buffered data.
    ///
    /// By that time, the buffer may have accumulated a large
    /// amount of data, causing it to be sent in large bursts,
    /// which introduces latency. To prevent this behavior and
    /// flatten data delivery to the peer, ntex's io can initiate
    /// out-of-order writes based on a configured threshold.
    ///
    /// Set `0` to disable send-buf.
    #[must_use]
    pub fn set_write_buf_threshold(mut self, size: usize) -> Self {
        self.write_buf_threshold = size;
        self
    }

    /// Set write buffer parameters.
    ///
    /// By default high watermark is set to 16Kb, low watermark 1kb.
    #[must_use]
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
    pub fn get(&self) -> BytesMut {
        if let Some(buf) =
            CACHE.with(|c| c.with(self.idx, self.first, |c: &mut Vec<_>| c.pop()))
        {
            buf
        } else {
            BytesMut::with_capacity(self.high)
        }
    }

    /// Get buffer with capacity
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity(cap)
    }

    #[inline]
    /// Resize buffer
    pub fn resize(&self, buf: &mut BytesMut) {
        if buf.remaining_mut() < self.low {
            self.resize_min(buf, self.high);
        }
    }

    #[inline]
    /// Resize buffer
    pub fn resize_min(&self, buf: &mut BytesMut, size: usize) {
        let mut avail = buf.remaining_mut();
        if avail < size {
            let mut new_cap = buf.capacity();
            while avail < size {
                avail += self.high;
                new_cap += self.high;
            }
            buf.reserve_capacity(new_cap);
        }
    }

    #[inline]
    /// Release buffer, buf must be allocated from this pool
    pub fn release(&self, mut buf: BytesMut) {
        let cap = buf.capacity();
        if cap > self.low && cap <= self.high {
            CACHE.with(|c| {
                c.with(self.idx, self.first, |v: &mut Vec<_>| {
                    if v.len() < self.cache_size {
                        buf.clear();
                        v.push(buf);
                    }
                });
            });
        }
    }
}

struct LocalCache {
    cache: UnsafeCell<Vec<(Vec<BytesMut>, Vec<BytesMut>)>>,
}

impl LocalCache {
    fn new() -> Self {
        Self {
            cache: UnsafeCell::new(Vec::with_capacity(16)),
        }
    }

    fn with<F, R>(&self, idx: usize, first: bool, f: F) -> R
    where
        F: FnOnce(&mut Vec<BytesMut>) -> R,
    {
        let cache = unsafe { &mut *self.cache.get() };

        while cache.len() <= idx {
            cache.push((Vec::new(), Vec::new()));
        }
        if first {
            f(&mut cache[idx].0)
        } else {
            f(&mut cache[idx].1)
        }
    }
}
