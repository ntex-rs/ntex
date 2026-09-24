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
    shutdown_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,
    write_timeout: Seconds,

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
    /// Byte-progress threshold that must be exceeded to extend the deadline.
    ///
    /// Progress must be strictly greater than this value for another `timeout`
    /// period to be granted.
    pub rate: u32,
}

/// Buffer allocation and backpressure thresholds.
#[derive(Copy, Clone, Debug)]
pub struct BufConfig {
    /// Buffered byte count at which backpressure is enabled.
    ///
    /// For [`IoConfig::read_buf`] this is also the capacity of a freshly
    /// allocated buffer, the growth increment used by
    /// [`resize_min`](Self::resize_min), and the free capacity guaranteed by
    /// [`resize`](Self::resize). For [`IoConfig::write_buf`] it is only a
    /// watermark; page sizing is controlled by
    /// [`IoConfig::set_write_page_size`].
    pub high: usize,
    /// Free-capacity threshold below which [`resize`](Self::resize) grows a
    /// buffer.
    ///
    /// This is the trigger for a resize, not the amount of free capacity the
    /// resize produces; see [`resize`](Self::resize).
    ///
    /// Buffers whose capacity is not greater than this value are not cached.
    ///
    /// This applies to [`IoConfig::read_buf`] only. Output is held in
    /// [`BytePages`](ntex_bytes::BytePages), which are neither resized nor
    /// cached this way, so the value is unused for [`IoConfig::write_buf`].
    pub low: usize,
    /// Outstanding byte count at which active backpressure is released.
    ///
    /// For [`IoConfig::write_buf`] this releases write backpressure, counting
    /// buffered output together with output a transport has taken ownership of
    /// but not yet written to the peer; for [`IoConfig::read_buf`] it releases
    /// read backpressure.
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
            shutdown_timeout: Seconds(1),
            frame_read_rate: None,
            write_timeout: Seconds(0),

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
    /// Returns the graceful shutdown timeout.
    pub fn shutdown_timeout(&self) -> Seconds {
        self.shutdown_timeout
    }

    #[inline]
    /// Returns the frame read-rate configuration.
    pub fn frame_read_rate(&self) -> Option<&FrameReadRate> {
        self.frame_read_rate.as_ref()
    }

    #[inline]
    /// Returns the write backpressure timeout.
    ///
    /// A zero value means the timeout is disabled.
    pub fn write_timeout(&self) -> Seconds {
        self.write_timeout
    }

    #[inline]
    /// Returns the read-buffer configuration.
    pub fn read_buf(&self) -> &BufConfig {
        &self.read_buf
    }

    #[inline]
    /// Returns the write-buffer configuration.
    ///
    /// Only the backpressure watermarks apply to output; see
    /// [`set_write_buf`](Self::set_write_buf).
    pub fn write_buf(&self) -> &BufConfig {
        &self.write_buf
    }

    #[inline]
    /// Returns the write-buffer page size.
    pub fn write_page_size(&self) -> BytePageSize {
        self.write_page_size
    }

    #[inline]
    /// Returns the buffered write size that triggers an earlier send.
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
    /// The dispatcher runs the timer only while the connection is idle: no
    /// input is buffered, no partial frame is being read, and no decoded
    /// frames are being handled. It starts once the last response is done.
    /// Partial frames are bounded by
    /// [frame read-rate](Self::set_frame_read_rate) limits instead, and write
    /// backpressure by the [write timeout](Self::set_write_timeout).
    ///
    /// A zero duration disables the timeout. It is disabled by default.
    #[must_use]
    pub fn set_keepalive_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        self.keepalive_timeout = timeout.into();
        self
    }

    /// Sets the graceful shutdown timeout.
    ///
    /// A graceful shutdown runs in two phases, and this single timeout bounds
    /// them together rather than applying to each one:
    ///
    /// 1. **Filter shutdown.** Both directions stay open, so a filter can emit
    ///    its closing data and still read the peer's. A TLS filter sends its
    ///    `close_notify` here, and a WebSocket filter its close frame.
    /// 2. **Transport shutdown.** The remaining output is drained to the peer.
    ///    The read side is paused, and whatever the peer still sent is
    ///    discarded, then the connection is closed.
    ///
    /// The deadline is armed when the first phase begins and is not restarted
    /// for the second, so a filter that shuts down slowly leaves less time to
    /// drain. Expiry in the first phase moves on to the second rather than
    /// giving up; only expiry in the second terminates the connection, and
    /// output that has not reached the transport is then lost. Either way
    /// [`crate::Io::shutdown`] reports a timed-out error once the transport
    /// has stopped.
    ///
    /// The timeout does not apply when there is nothing to drain, so a
    /// connection with no pending output never fails on it.
    ///
    /// The default is one second.
    ///
    /// # Panics
    ///
    /// Panics if `timeout` is zero. Without a deadline, a peer that never
    /// completes the exchange, or never reads the remaining output, could
    /// hold the connection open forever.
    #[must_use]
    pub fn set_shutdown_timeout<T: Into<Seconds>>(mut self, timeout: T) -> Self {
        let timeout = timeout.into();
        assert!(
            timeout.non_zero(),
            "shutdown timeout must be greater than zero"
        );
        self.shutdown_timeout = timeout;
        self
    }

    /// Sets read-rate parameters for a single decoded frame.
    ///
    /// Rate tracking starts when a new connection arrives, for its first
    /// frame, and later whenever a decoder returns no complete item after
    /// receiving partial frame data, whether the data is left in the read
    /// buffer or consumed into the decoder's own state. The dispatcher then
    /// allows one `timeout` period for additional data to arrive.
    ///
    /// When that period expires, the dispatcher compares the bytes received
    /// for the frame since the previous check with `rate`:
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
    /// is maintained. A non-zero value is enforced in whole `timeout` periods,
    /// so the effective limit is rounded up to a multiple of `timeout` and is
    /// never shorter than the initial period.
    ///
    /// A zero `timeout` disables frame read-rate enforcement and ignores
    /// `max_timeout` and `rate`. With a non-zero timeout and `rate` set to zero,
    /// any positive byte progress permits another period.
    ///
    /// A new connection must therefore deliver its first frame within these
    /// limits. After a frame has been decoded, idle connections with no
    /// partial frame are governed separately by
    /// [`set_keepalive_timeout`](Self::set_keepalive_timeout).
    ///
    /// The timer is suspended while write backpressure is active; the elapsed
    /// part of the period is charged to `max_timeout`.
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

    /// Sets the write backpressure timeout.
    ///
    /// Write backpressure is enabled when outstanding output reaches the
    /// [write buffer](Self::set_write_buf) high watermark and disabled once the
    /// peer has accepted enough of it. The timeout covers that whole period:
    /// if backpressure is still enabled when it expires, the dispatcher stops
    /// with a write timeout. Each backpressure period starts a fresh timeout,
    /// however much the peer read during the previous one. Without a write
    /// timeout, a peer that stops reading during backpressure can hold the
    /// connection open indefinitely.
    ///
    /// A zero duration disables the timeout. It is disabled by default.
    #[must_use]
    pub fn set_write_timeout(mut self, timeout: Seconds) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Sets read-buffer watermarks and cache capacity.
    ///
    /// `high_watermark` enables read backpressure when the application-facing
    /// buffer reaches this size. It is also the capacity of a freshly
    /// allocated read buffer, the increment by which buffers grow, and the
    /// free capacity a resize guarantees. It must be greater than zero.
    /// `low_watermark` is the free-capacity threshold below which a read
    /// buffer is grown. `cache_size` limits the number of eligible buffers
    /// retained per thread and configuration.
    ///
    /// Read backpressure is released once the application-facing buffer falls
    /// to half of `high_watermark`.
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

    /// Sets the write-buffer backpressure watermark.
    ///
    /// `high_watermark` enables write backpressure at this outstanding size and
    /// must be greater than zero. Backpressure is released after the
    /// outstanding size falls to half of this value. Outstanding output is the
    /// buffered output plus any output a transport has taken ownership of but
    /// not yet written to the peer.
    ///
    /// Unlike [`set_read_buf`](Self::set_read_buf) this takes no low watermark
    /// or cache size. Output is held in [`BytePages`](ntex_bytes::BytePages),
    /// which are sized by [`set_write_page_size`](Self::set_write_page_size)
    /// and are not served from the read-buffer cache.
    ///
    /// By default, the high watermark is approximately 16 KiB.
    ///
    /// # Panics
    ///
    /// Panics if `high_watermark` is zero.
    #[must_use]
    pub fn set_write_buf(mut self, high_watermark: usize) -> Self {
        assert!(
            high_watermark > 0,
            "write buffer high watermark must be greater than zero"
        );
        self.write_buf.high = high_watermark;
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
    /// Ensures that the buffer has at least `high` bytes of free capacity.
    ///
    /// The buffer is grown only when its free capacity has fallen below `low`;
    /// `low` is the trigger for the resize, while `high` is the amount of free
    /// capacity the resize guarantees.
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
            .set_write_buf(2048);

        assert_eq!(cfg.read_buf().high, 1024);
        assert_eq!(cfg.read_buf().low, 128);
        assert_eq!(cfg.read_buf().half, 512);
        assert_eq!(cfg.write_buf().high, 2048);
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
    fn write_timeout_configuration() {
        let cfg = IoConfig::new();
        assert!(cfg.write_timeout().is_zero());

        let cfg = cfg.set_write_timeout(Seconds(3));
        assert_eq!(cfg.write_timeout(), Seconds(3));

        let cfg = cfg.set_write_timeout(Seconds::ZERO);
        assert!(cfg.write_timeout().is_zero());
    }

    #[test]
    #[should_panic(expected = "read buffer high watermark must be greater than zero")]
    fn zero_read_high_watermark() {
        let _ = IoConfig::new().set_read_buf(0, 128, 4);
    }

    #[test]
    #[should_panic(expected = "write buffer high watermark must be greater than zero")]
    fn zero_write_high_watermark() {
        let _ = IoConfig::new().set_write_buf(0);
    }

    #[test]
    #[should_panic(expected = "buffer high watermark must be greater than zero")]
    fn zero_resize_increment() {
        let mut cfg = *IoConfig::new().read_buf();
        cfg.high = 0;
        cfg.resize_min(&mut BytesMut::new(), 1024);
    }

    #[test]
    #[should_panic(expected = "shutdown timeout must be greater than zero")]
    fn zero_shutdown_timeout_is_rejected() {
        let _ = IoConfig::new().set_shutdown_timeout(Seconds::ZERO);
    }
}
