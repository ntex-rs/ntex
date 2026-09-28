//! I/O buffer, timeout, and frame-rate configuration.

use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

use ntex_bytes::{BytePageSize, BytesMut, buf::BufMut};
use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{time::Millis, time::Seconds};

const DEFAULT_CACHE_LIMIT: usize = 1024 * 1024;
const DEFAULT_HIGH: usize = 16 * 1024 - 24;
const DEFAULT_LOW: usize = 512 + 24;
const DEFAULT_HALF: usize = (16 * 1024 - 24) / 2;
// read buffers above `high` double in capacity, by at most this much at once
const MAX_GROW_STEP: usize = 1024 * 1024;

static CACHE_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_CACHE_LIMIT);

thread_local! {
    static CACHE: LocalCache = LocalCache::new();
}

/// Sets the most read-buffer capacity, in bytes, each thread keeps cached.
///
/// Every thread keeps one cache of empty read buffers, shared by all
/// configurations. Once the capacity of the cached buffers exceeds this
/// limit, the least recently released buffers are freed. A thread applies a
/// new limit the next time it releases a buffer. Zero disables the cache.
///
/// The default is 1 MiB.
pub fn set_read_buf_cache_limit(limit: usize) {
    CACHE_LIMIT.store(limit, Ordering::Relaxed);
}

/// Returns the per-thread read-buffer cache limit, in bytes.
///
/// See [`set_read_buf_cache_limit`].
pub fn read_buf_cache_limit() -> usize {
    CACHE_LIMIT.load(Ordering::Relaxed)
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
#[non_exhaustive]
pub struct BufConfig {
    /// Buffered byte count at which backpressure is enabled.
    ///
    /// For [`IoConfig::read_buf`] this is also the capacity of a freshly
    /// allocated buffer, the capacity [`resize`](Self::resize) compacts
    /// buffered data into, and the largest capacity that is cached. For
    /// [`IoConfig::write_buf`] it is only a
    /// watermark; page sizing is controlled by
    /// [`IoConfig::set_write_page_size`].
    pub high: usize,
    /// Free-capacity threshold below which [`resize`](Self::resize) grows a
    /// buffer.
    ///
    /// This is the trigger for a resize, and the least free capacity a resize
    /// that compacts the buffer produces; see [`resize`](Self::resize).
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
}

impl IoConfig {
    #[inline]
    #[must_use]
    /// Creates an I/O configuration with default settings.
    pub fn new() -> IoConfig {
        IoConfig {
            config: CfgContext::default(),
            connect_timeout: Millis::ZERO,
            keepalive_timeout: Seconds(0),
            shutdown_timeout: Seconds(1),
            frame_read_rate: None,
            write_timeout: Seconds(0),

            read_buf: BufConfig {
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
            },
            write_buf: BufConfig {
                high: DEFAULT_HIGH,
                low: DEFAULT_LOW,
                half: DEFAULT_HALF,
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
    /// The timer is stopped while write backpressure is active, when frames
    /// are not decoded, and a new period starts once decoding resumes. While
    /// the service is not ready the timer is stopped as well, and tracking
    /// restarts with a fresh period and `max_timeout` budget once the service
    /// is ready.
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
    /// The timeout does not apply once backpressure is disabled, even though
    /// output is still outstanding. A peer that stops reading at that point
    /// can leave up to half of the high watermark unwritten; only the
    /// [keep-alive timeout](Self::set_keepalive_timeout), when enabled, bounds
    /// such a connection until it is shut down.
    ///
    /// A zero duration disables the timeout. It is disabled by default.
    #[must_use]
    pub fn set_write_timeout(mut self, timeout: Seconds) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Sets read-buffer watermarks.
    ///
    /// `high_watermark` enables read backpressure when the application-facing
    /// buffer reaches this size. It is also the capacity of a freshly
    /// allocated read buffer and the capacity buffered data is compacted into
    /// when free capacity runs low; larger data grows the buffer by doubling
    /// its capacity. It must be greater than zero.
    /// `low_watermark` is the free-capacity threshold below which a read
    /// buffer is compacted or grown.
    ///
    /// Empty read buffers are kept in a per-thread cache shared by all
    /// configurations, see [`set_read_buf_cache_limit`]. A connection holding
    /// unconsumed input, such as the start of a frame that has not fully
    /// arrived, keeps its whole read buffer, so each such connection uses at
    /// least `high_watermark` bytes until the rest arrives. Read-rate timeouts,
    /// see [`set_frame_read_rate`](Self::set_frame_read_rate), bound how long
    /// a slow peer can hold it.
    ///
    /// Frames that a codec splits off the read buffer, such as `Bytes`
    /// payloads, share its allocation. A frame kept alive keeps the whole
    /// read buffer allocated, and the buffer is not returned to the cache, so
    /// retaining many small frames can use far more memory than their size.
    /// Copy long-lived frames or call [`Bytes::trimdown`](ntex_bytes::Bytes::trimdown)
    /// on them to release the rest of the buffer.
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
    pub fn set_read_buf(mut self, high_watermark: usize, low_watermark: usize) -> Self {
        assert!(
            high_watermark > 0,
            "read buffer high watermark must be greater than zero"
        );
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
    /// Unlike [`set_read_buf`](Self::set_read_buf) this takes no low watermark. Output is held in [`BytePages`](ntex_bytes::BytePages),
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
    /// Acquires an empty buffer from the thread-local cache.
    ///
    /// Returns the most recently released buffer that is eligible for this
    /// configuration, see [`release`](Self::release). If there is none,
    /// allocates a buffer with capacity `high`.
    pub fn get(&self) -> BytesMut {
        if let Some(buf) = CACHE.with(|c| c.get(self)) {
            buf
        } else {
            BytesMut::with_capacity(self.high)
        }
    }

    fn is_cacheable(&self, cap: usize) -> bool {
        cap > self.low && cap <= self.high
    }

    /// Creates a new uncached buffer with the specified capacity.
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity(cap)
    }

    #[inline]
    /// Makes room for another read once free capacity falls below `low`.
    ///
    /// When the buffered data plus `low` fits into `high`, the data is moved
    /// into a buffer of capacity `high`, so the free capacity afterwards is
    /// `high` minus the buffered length. Only larger data grows the buffer
    /// beyond `high`, in which case at least `high` bytes are free afterwards.
    pub fn resize(&self, buf: &mut BytesMut) {
        if buf.remaining_mut() < self.low {
            if buf.len() + self.low <= self.high {
                self.resize_min(buf, self.low);
            } else {
                self.resize_min(buf, self.high);
            }
        }
    }

    #[inline]
    /// Ensures that the buffer has at least `size` bytes of remaining capacity.
    ///
    /// When the buffered data plus `size` fits into `high`, the data is moved
    /// into a cached buffer of capacity `high`, and the old buffer is returned
    /// to the cache. Otherwise the buffer grows to double its capacity,
    /// growing by at most 1 MiB at once, or to enough capacity for `size` more
    /// bytes if that is larger. A buffer that is not shared with split-off
    /// data is compacted in place when its allocation is large enough, or is
    /// reallocated, often without copying. Buffers grown beyond `high` are
    /// never cached.
    ///
    /// # Panics
    ///
    /// Panics if growth is required and `high` is zero.
    pub fn resize_min(&self, buf: &mut BytesMut, size: usize) {
        let avail = buf.remaining_mut();
        if avail < size {
            assert!(
                self.high > 0,
                "buffer high watermark must be greater than zero"
            );
            if buf.len() + size <= self.high {
                let mut new_buf = self.get();
                if new_buf.capacity() < self.high {
                    new_buf = BytesMut::with_capacity(self.high);
                }
                new_buf.extend_from_slice(buf);
                self.release(std::mem::replace(buf, new_buf));
                return;
            }

            let len = buf.len();
            let cap = buf.capacity();
            let new_cap = (len + size).max(cap + cap.min(MAX_GROW_STEP));
            // a unique buffer is compacted in place when its allocation holds
            // `new_cap` bytes, or grown with a reallocation
            buf.reserve_exact(new_cap - len);
        }
    }

    #[inline]
    /// Returns an eligible buffer to the thread-local cache.
    ///
    /// The buffer is retained only when its capacity is greater than `low` and
    /// no greater than `high`, and no other handle refers to its allocation.
    /// A buffer that still shares its allocation with split-off data is
    /// dropped instead: its capacity covers only the part after that data, but
    /// caching it would keep the whole allocation alive. If the cache then
    /// holds more capacity than [`read_buf_cache_limit`], the least recently
    /// released buffers are freed.
    pub fn release(&self, mut buf: BytesMut) {
        // Uniqueness can only be gained, never lost, so a buffer that is
        // unique here is fully reclaimed by `clear()`.
        if buf.is_unique() {
            buf.clear();
            if self.is_cacheable(buf.capacity()) {
                CACHE.with(|c| c.release(buf));
            }
        }
    }
}

struct LocalCache {
    bufs: UnsafeCell<VecDeque<BytesMut>>,
    size: Cell<usize>,
}

impl LocalCache {
    fn new() -> Self {
        Self {
            bufs: UnsafeCell::new(VecDeque::new()),
            size: Cell::new(0),
        }
    }

    fn get(&self, cfg: &BufConfig) -> Option<BytesMut> {
        // SAFETY: the cache is thread-local and never borrowed across calls
        let bufs = unsafe { &mut *self.bufs.get() };
        let pos = bufs.iter().rposition(|b| cfg.is_cacheable(b.capacity()))?;
        let buf = bufs.remove(pos)?;
        self.size.set(self.size.get() - buf.capacity());
        Some(buf)
    }

    fn release(&self, buf: BytesMut) {
        // SAFETY: the cache is thread-local and never borrowed across calls
        let bufs = unsafe { &mut *self.bufs.get() };
        let limit = read_buf_cache_limit();
        let mut size = self.size.get() + buf.capacity();
        bufs.push_back(buf);
        while size > limit {
            let Some(buf) = bufs.pop_front() else { break };
            size -= buf.capacity();
        }
        self.size.set(size);
    }

    #[cfg(test)]
    fn clear(&self) {
        // SAFETY: the cache is thread-local and never borrowed across calls
        unsafe { (*self.bufs.get()).clear() };
        self.size.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_configuration() {
        let cfg = IoConfig::new().set_read_buf(1024, 128).set_write_buf(2048);

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
        let _ = IoConfig::new().set_read_buf(0, 128);
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
    fn resize_compacts_into_cached_buffer() {
        let cfg = *IoConfig::new().set_read_buf(4096, 512).read_buf();

        // leftover input at the end of a consumed buffer
        let mut buf = cfg.get();
        buf.extend_from_slice(&[1; 4000]);
        let _ = buf.split_to(3900);
        assert!(buf.remaining_mut() < cfg.low);

        cfg.resize(&mut buf);
        assert_eq!(&buf[..], &[1; 100][..]);
        assert_eq!(buf.capacity(), cfg.high);
        assert_eq!(buf.remaining_mut(), cfg.high - 100);

        // the compacted buffer can be cached again
        buf.clear();
        cfg.release(buf);
        let buf = cfg.get();
        assert_eq!(buf.capacity(), cfg.high);

        // an explicit minimum that fits is compacted too
        let mut buf = cfg.get();
        buf.extend_from_slice(&[2; 3000]);
        cfg.resize_min(&mut buf, 1000);
        assert_eq!(buf.capacity(), cfg.high);
        assert_eq!(&buf[..], &[2; 3000][..]);

        // data that does not fit grows the buffer
        let mut buf = cfg.get();
        buf.extend_from_slice(&[3; 3800]);
        cfg.resize(&mut buf);
        assert_eq!(buf.len(), 3800);
        assert!(buf.remaining_mut() >= cfg.high);
    }

    #[test]
    fn cache_is_shared_by_configs() {
        CACHE.with(LocalCache::clear);
        let a = *IoConfig::new().read_buf();
        let b = *IoConfig::new().set_read_buf(DEFAULT_HIGH, 1024).read_buf();

        let buf = a.get();
        let ptr = buf.as_ptr();
        a.release(buf);
        let buf = b.get();
        assert_eq!(buf.as_ptr(), ptr, "buffer released by another config");

        // a buffer not eligible for the requesting config stays cached
        let small = *IoConfig::new().set_read_buf(4096, 512).read_buf();
        b.release(buf);
        let buf = small.get();
        assert_ne!(buf.as_ptr(), ptr);
        assert_eq!(b.get().as_ptr(), ptr);
        drop(buf);
    }

    #[test]
    fn cache_is_bounded_by_capacity() {
        CACHE.with(LocalCache::clear);
        let cfg = *IoConfig::new().read_buf();
        let limit = read_buf_cache_limit();
        assert_eq!(limit, DEFAULT_CACHE_LIMIT);

        let bufs: Vec<_> = (0..limit / cfg.high + 8).map(|_| cfg.get()).collect();
        let ptrs: Vec<_> = bufs.iter().map(|b| b.as_ptr()).collect();
        for buf in bufs {
            cfg.release(buf);
        }
        let size = CACHE.with(|c| c.size.get());
        assert!(size <= limit, "cache holds {size} bytes");
        assert!(size + cfg.high > limit);

        // the oldest buffers were freed, the newest are handed out first
        let n = size / cfg.high;
        for ptr in ptrs.iter().rev().take(n) {
            assert_eq!(cfg.get().as_ptr(), *ptr);
        }
        assert_eq!(CACHE.with(|c| c.size.get()), 0);
    }

    #[test]
    fn large_buffers_grow_by_doubling_and_are_not_cached() {
        CACHE.with(LocalCache::clear);
        let cfg = *IoConfig::new().read_buf();

        // reads that fill the buffer until a 1 MiB frame is buffered
        let mut buf = cfg.get();
        let mut grows = 0;
        while buf.len() < 1024 * 1024 {
            let (ptr, cap) = (buf.as_ptr(), buf.capacity());
            cfg.resize(&mut buf);
            if buf.as_ptr() != ptr {
                grows += 1;
                assert!(buf.capacity() >= 2 * cap, "{cap} -> {}", buf.capacity());
            }
            let n = buf.remaining_mut();
            buf.extend_from_slice(&vec![1; n]);
        }
        assert!(grows <= 8, "{grows} reallocations");
        assert!(buf.capacity() <= 2 * 1024 * 1024 + cfg.high);

        // the step is limited for very large buffers
        let mut big = BytesMut::with_capacity(4 * MAX_GROW_STEP);
        big.extend_from_slice(&vec![2; 4 * MAX_GROW_STEP]);
        cfg.resize_min(&mut big, 1);
        assert_eq!(big.capacity(), 5 * MAX_GROW_STEP);
        drop(big);

        // grown buffers are dropped, even once most of the data is consumed
        let len = buf.len();
        drop(buf.split_to(len - 10));
        assert!(buf.is_unique());
        cfg.release(buf);
        assert_eq!(CACHE.with(|c| c.size.get()), 0);
    }

    #[test]
    fn large_unique_buffer_is_compacted_in_place() {
        let cfg = *IoConfig::new().read_buf();
        let cap = 16 * cfg.high;

        // most of a large buffer is consumed, the rest is not shared
        let mut buf = BytesMut::with_capacity(cap);
        let base = buf.as_ptr();
        buf.extend_from_slice(&vec![1; cap]);
        drop(buf.split_to(cap - 2 * cfg.high));
        assert!(buf.is_unique());
        assert_eq!(buf.remaining_mut(), 0);

        cfg.resize_min(&mut buf, cfg.high);
        assert_eq!(buf.as_ptr(), base);
        assert_eq!(buf.capacity(), cap);
        assert_eq!(&buf[..], &vec![1; 2 * cfg.high][..]);

        // a shared buffer is copied into a new allocation
        let mut buf = BytesMut::with_capacity(cap);
        buf.extend_from_slice(&vec![2; cap]);
        let front = buf.split_to(cap - 2 * cfg.high);
        cfg.resize_min(&mut buf, cfg.high);
        assert!(buf.remaining_mut() >= cfg.high);
        assert_eq!(&buf[..], &vec![2; 2 * cfg.high][..]);
        assert_eq!(&front[..], &vec![2; cap - 2 * cfg.high][..]);
    }

    #[test]
    fn shared_buffer_is_not_cached() {
        CACHE.with(LocalCache::clear);
        let cfg = *IoConfig::new().read_buf();

        // a decoded frame still refers to the start of the buffer
        let mut buf = cfg.get();
        buf.extend_from_slice(&vec![1; cfg.high - 1000]);
        let frame = buf.split_to(buf.len());
        assert!(cfg.is_cacheable(buf.capacity()));
        cfg.release(buf);
        assert_eq!(CACHE.with(|c| c.size.get()), 0);

        // once the frame is gone the whole buffer is reclaimed and cached
        let mut buf = cfg.get();
        buf.extend_from_slice(&vec![2; cfg.high - 1000]);
        let frame2 = buf.split_to(buf.len());
        drop(frame2);
        cfg.release(buf);
        assert_eq!(CACHE.with(|c| c.size.get()), cfg.high);
        assert_eq!(cfg.get().capacity(), cfg.high);
        drop(frame);
    }

    #[test]
    #[should_panic(expected = "shutdown timeout must be greater than zero")]
    fn zero_shutdown_timeout_is_rejected() {
        let _ = IoConfig::new().set_shutdown_timeout(Seconds::ZERO);
    }
}
