//! I/O buffer, timeout, and frame-rate configuration.

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
/// Shared configuration for an [`crate::Io`] stream.
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

/// Minimum read rate required while decoding one frame.
#[derive(Copy, Clone, Debug)]
pub struct FrameReadRate {
    /// Initial read timeout.
    pub timeout: Seconds,
    /// Maximum cumulative timeout for the frame.
    pub max_timeout: Seconds,
    /// Number of bytes that extends the deadline by one `timeout` period.
    pub rate: u32,
}

/// Buffer allocation and backpressure thresholds.
#[derive(Copy, Clone, Debug)]
pub struct BufConfig {
    /// Buffered byte count at which backpressure is enabled.
    pub high: usize,
    /// Minimum free capacity requested when resizing a read buffer.
    ///
    /// Buffers whose capacity is not greater than this value are not cached.
    pub low: usize,
    /// Buffered byte count at which active write backpressure is released.
    ///
    /// This is set to half of `high` by the configuration builders.
    pub half: usize,
    idx: usize,
    first: bool,
    cache_size: usize,
}

impl IoConfig {
    #[inline]
    #[must_use]
    /// Creates an I/O configuration with default settings.
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
    /// Returns the shared configuration tag.
    pub fn tag(&self) -> &str {
        self.config.tag()
    }

    #[inline]
    /// Returns the connection timeout.
    pub fn connect_timeout(&self) -> Millis {
        self.connect_timeout
    }

    #[inline]
    /// Returns the keep-alive timeout.
    pub fn keepalive_timeout(&self) -> Seconds {
        self.keepalive_timeout
    }

    #[inline]
    /// Returns the graceful disconnect timeout.
    pub fn disconnect_timeout(&self) -> Seconds {
        self.disconnect_timeout
    }

    #[inline]
    /// Returns the frame read-rate configuration.
    pub fn frame_read_rate(&self) -> Option<&FrameReadRate> {
        self.frame_read_rate.as_ref()
    }

    #[inline]
    /// Returns the read-buffer configuration.
    pub fn read_buf(&self) -> &BufConfig {
        &self.read_buf
    }

    #[inline]
    /// Returns the write-buffer configuration.
    pub fn write_buf(&self) -> &BufConfig {
        &self.write_buf
    }

    #[inline]
    /// Returns the write-buffer page size.
    pub fn write_page_size(&self) -> BytePageSize {
        self.write_page_size
    }

    #[inline]
    /// The write buffer threshold that triggers earlier sending.
    pub fn write_buf_threshold(&self) -> usize {
        self.write_buf_threshold
    }

    /// Sets the connection timeout.
    ///
    /// A zero duration disables the timeout. It is disabled by default.
    #[must_use]
    pub fn set_connect_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.connect_timeout = timeout.into();
        self
    }

    /// Sets the keep-alive timeout.
    ///
    /// A zero duration disables the timeout. It is disabled by default.
    #[must_use]
    pub fn set_keepalive_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.keepalive_timeout = timeout.into();
        self
    }

    /// Sets the graceful disconnect timeout.
    ///
    /// If filter shutdown does not complete within this duration, transport
    /// teardown continues and [`crate::Io::shutdown`] returns a timed-out
    /// error after the transport has stopped.
    ///
    /// A zero duration disables the timeout. The default is one second.
    #[must_use]
    pub fn set_disconnect_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.disconnect_timeout = timeout.into();
        self
    }

    /// Sets read-rate parameters for a single decoded frame.
    ///
    /// Rate tracking starts when a dispatcher begins waiting for the first
    /// frame on a new connection. After a frame completes, tracking starts
    /// again when bytes for the next frame arrive.
    ///
    /// When that period expires, the dispatcher compares the received-byte
    /// progress since the previous check with `rate`. Progress counts bytes
    /// received from the transport even when the decoder consumes them while
    /// waiting for the rest of the frame:
    ///
    /// - If the progress is greater than `rate`, the deadline is extended by
    ///   another `timeout` period.
    /// - If the progress is at most `rate`, frame decoding fails with a read
    ///   timeout.
    /// - Completing the frame clears the timer and resets rate tracking for the
    ///   next frame.
    ///
    /// `max_timeout` limits the cumulative time allowed for one frame. A zero
    /// value permits an unlimited number of extensions while the required rate
    /// is maintained. When `max_timeout` is not an exact multiple of `timeout`,
    /// the final measurement interval is shortened so the cumulative limit is
    /// not exceeded.
    ///
    /// A zero `timeout` disables frame read-rate enforcement and ignores
    /// `max_timeout` and `rate`. With a non-zero timeout and `rate` set to zero,
    /// any positive buffered-byte progress permits another period.
    ///
    /// After the first frame, idle time between frames is governed separately
    /// by [`set_keepalive_timeout`](Self::set_keepalive_timeout).
    ///
    /// The dispatcher pauses frame timing while service readiness prevents it
    /// from reading. Resuming preserves the unused part of the active interval
    /// and does not grant a new cumulative `max_timeout` budget.
    ///
    /// Frame read-rate enforcement is disabled by default.
    #[must_use]
    pub fn set_frame_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> Self {
        self.frame_read_rate = if timeout.is_zero() {
            None
        } else {
            Some(FrameReadRate {
                timeout,
                max_timeout,
                rate,
            })
        };
        self
    }

    /// Sets read-buffer watermarks and cache capacity.
    ///
    /// `high_watermark` enables read backpressure when the application-facing
    /// buffer reaches this size and is also used as the allocation growth
    /// increment. It must be greater than zero. `low_watermark` is the minimum
    /// free capacity requested when resizing a read buffer. `cache_size` limits
    /// the number of eligible buffers retained per thread and configuration.
    ///
    /// By default, the high watermark is approximately 16 KiB and the low
    /// watermark is approximately 512 bytes.
    ///
    /// # Panics
    ///
    /// Panics if `high_watermark` is zero.
    #[must_use]
    pub fn set_read_buf(
        mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> Self {
        assert!(
            high_watermark > 0,
            "read buffer high watermark must be greater than zero"
        );
        self.read_buf.cache_size = cache_size;
        self.read_buf.high = high_watermark;
        self.read_buf.low = low_watermark;
        self.read_buf.half = high_watermark >> 1;
        self
    }

    /// Sets the write-buffer page size.
    ///
    /// Write buffers are represented as a sequence of reusable byte pages.
    /// `size` selects the capacity category used when those buffers allocate
    /// new internal pages, including the intermediate write buffers created
    /// between filter layers. [`BytePages`](ntex_bytes::BytePages) may also
    /// contain externally supplied [`BytePage`](ntex_bytes::BytePage),
    /// [`Bytes`](ntex_bytes::Bytes), or `Vec<u8>` segments; this setting does
    /// not resize or copy those segments.
    ///
    /// Smaller pages reduce unused capacity for connections that usually
    /// produce small writes. Larger pages can reduce allocation and page-list
    /// overhead for connections that regularly buffer larger writes. This
    /// setting does not limit the total amount of buffered data or determine
    /// the size of individual transport write operations.
    ///
    /// Changing the page size on an active connection through
    /// [`Io::set_config`](crate::Io::set_config) updates the allocation
    /// category for future pages in every existing write buffer. Pages that
    /// have already been allocated retain their current capacity. Filter
    /// layers added later use the new page size.
    ///
    /// The page size is independent of the eager-write threshold and write
    /// backpressure watermarks. Changing it does not update values configured
    /// by [`set_write_buf_threshold`](Self::set_write_buf_threshold) or
    /// [`set_write_buf`](Self::set_write_buf).
    ///
    /// The default page size is 16 KiB.
    #[must_use]
    pub fn set_write_page_size(mut self, size: BytePageSize) -> Self {
        self.write_page_size = size;
        self
    }

    /// Sets the write buffer threshold.
    ///
    /// Application code can encode multiple items during one dispatcher turn.
    /// Normally, the transport write task is scheduled to run after that work
    /// yields, so all items produced during the turn may accumulate in the
    /// write buffer and be sent as one large burst.
    ///
    /// When the buffered size reaches `size` while the transport write task is
    /// paused, `Io` asks the transport handle to start writing immediately.
    /// This eager write attempt occurs synchronously with buffer consolidation,
    /// before the current application or dispatcher turn necessarily
    /// completes. If bytes remain afterward, the normal write task is
    /// scheduled to continue delivery. Data below the threshold is delivered
    /// through that normal scheduled path.
    ///
    /// The threshold is a latency and write-burst tuning parameter. It does not
    /// limit write-buffer growth, provide a flush guarantee, or control write
    /// backpressure; use [`set_write_buf`](Self::set_write_buf) for
    /// backpressure watermarks and [`Io::flush`](crate::Io::flush) when a
    /// caller must wait for buffered data to be written.
    ///
    /// Set `size` to zero to disable eager writes when this configuration is
    /// used to construct an [`Io`](crate::Io). The default is 8 KiB, derived
    /// from the default 16 KiB page size when [`IoConfig::new`] is called.
    /// Changing the page size later does not recalculate this threshold.
    ///
    /// Replacing an active connection's configuration with
    /// [`Io::set_config`](crate::Io::set_config) enables or disables eager
    /// writes according to the replacement threshold.
    #[must_use]
    pub fn set_write_buf_threshold(mut self, size: usize) -> Self {
        self.write_buf_threshold = size;
        self
    }

    /// Sets write-buffer watermarks and cache capacity.
    ///
    /// `high_watermark` enables write backpressure at this buffered size and
    /// must be greater than zero. Backpressure is released after the buffered
    /// size falls to half of this value. `low_watermark` controls which empty
    /// buffers are eligible for caching, and `cache_size` limits the number
    /// retained per thread and configuration.
    ///
    /// By default, the high watermark is approximately 16 KiB and the low
    /// watermark is approximately 512 bytes.
    ///
    /// # Panics
    ///
    /// Panics if `high_watermark` is zero.
    #[must_use]
    pub fn set_write_buf(
        mut self,
        high_watermark: usize,
        low_watermark: usize,
        cache_size: usize,
    ) -> Self {
        assert!(
            high_watermark > 0,
            "write buffer high watermark must be greater than zero"
        );
        self.write_buf.cache_size = cache_size;
        self.write_buf.high = high_watermark;
        self.write_buf.low = low_watermark;
        self.write_buf.half = high_watermark >> 1;
        self
    }
}

impl BufConfig {
    #[inline]
    /// Acquires an empty buffer from this configuration's thread-local cache.
    ///
    /// If the cache is empty, allocates a buffer with capacity `high`.
    pub fn get(&self) -> BytesMut {
        if let Some(buf) = CACHE.with(|c| c.with(self.idx, self.first, |c: &mut Vec<_>| c.pop())) {
            buf
        } else {
            BytesMut::with_capacity(self.high)
        }
    }

    /// Creates a new uncached buffer with the specified capacity.
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity(cap)
    }

    #[inline]
    /// Ensures that the buffer has at least the configured low watermark free.
    pub fn resize(&self, buf: &mut BytesMut) {
        if buf.remaining_mut() < self.low {
            self.resize_min(buf, self.high);
        }
    }

    #[inline]
    /// Ensures that the buffer has at least `size` bytes of remaining capacity.
    ///
    /// # Panics
    ///
    /// Panics if growth is required and `high` is zero.
    pub fn resize_min(&self, buf: &mut BytesMut, size: usize) {
        let mut avail = buf.remaining_mut();
        if avail < size {
            assert!(
                self.high > 0,
                "buffer high watermark must be greater than zero"
            );
            let mut new_cap = buf.capacity();
            while avail < size {
                avail += self.high;
                new_cap += self.high;
            }
            buf.reserve_capacity(new_cap);
        }
    }

    #[inline]
    /// Returns an eligible buffer to this configuration's thread-local cache.
    ///
    /// The buffer is retained only when its capacity is greater than `low`, no
    /// greater than `high`, and this configuration's cache is not full.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_configuration() {
        let cfg = IoConfig::new()
            .set_read_buf(1024, 128, 4)
            .set_write_buf(2048, 256, 8);

        assert_eq!(cfg.read_buf().high, 1024);
        assert_eq!(cfg.read_buf().low, 128);
        assert_eq!(cfg.read_buf().half, 512);
        assert_eq!(cfg.write_buf().high, 2048);
        assert_eq!(cfg.write_buf().low, 256);
        assert_eq!(cfg.write_buf().half, 1024);
    }

    #[test]
    fn frame_read_rate_configuration() {
        let cfg = IoConfig::new().set_frame_read_rate(Seconds(1), Seconds(3), 128);
        let rate = cfg.frame_read_rate().unwrap();
        assert_eq!(rate.timeout, Seconds(1));
        assert_eq!(rate.max_timeout, Seconds(3));
        assert_eq!(rate.rate, 128);

        let cfg = cfg.set_frame_read_rate(Seconds::ZERO, Seconds(10), 1024);
        assert!(cfg.frame_read_rate().is_none());
    }

    #[test]
    #[should_panic(expected = "read buffer high watermark must be greater than zero")]
    fn zero_read_high_watermark() {
        let _ = IoConfig::new().set_read_buf(0, 128, 4);
    }

    #[test]
    #[should_panic(expected = "write buffer high watermark must be greater than zero")]
    fn zero_write_high_watermark() {
        let _ = IoConfig::new().set_write_buf(0, 128, 4);
    }

    #[test]
    #[should_panic(expected = "buffer high watermark must be greater than zero")]
    fn zero_resize_increment() {
        let mut cfg = *IoConfig::new().read_buf();
        cfg.high = 0;
        cfg.resize_min(&mut BytesMut::new(), 1024);
    }
}
