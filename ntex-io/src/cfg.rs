//! I/O buffer, timeout, and frame-rate configuration.

use ntex_bytes::{BytePageSize, BytesMut, METADATA_SIZE, buf::BufMut};
use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{time::Millis, time::Seconds};

const DEFAULT_HIGH: usize = 16 * 1024 - METADATA_SIZE;
const DEFAULT_LOW: usize = 512 + 24;
const DEFAULT_HALF: usize = (16 * 1024 - METADATA_SIZE) / 2;
// buffers beyond the largest page size double in capacity, by at most this
// much at once
const MAX_GROW_STEP: usize = 1024 * 1024;

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
    /// For [`IoConfig::read_buf`] this also selects the page size read
    /// buffers are allocated with: the smallest [`BytePageSize`] that holds
    /// `high` bytes. A buffer is at least 4 KiB, and its capacity can be larger
    /// than `high`. Above the largest page size, read buffers have
    /// exactly `high` bytes of capacity and are not cached. For
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
    /// This applies to [`IoConfig::read_buf`] only. Output is held in
    /// [`BytePages`](ntex_bytes::BytePages), which are not resized this way,
    /// so the value is unused for [`IoConfig::write_buf`].
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
    /// Reads paused because output produced by reading, for example replies
    /// to peer pings, has not drained are not covered either. While the
    /// dispatcher is idle, the keep-alive timeout bounds them. Application
    /// output written during such a pause enables write backpressure.
    ///
    /// Outside the dispatcher, the timeout also bounds each wait for output in
    /// [`Io::send`](crate::Io::send), [`Io::flush`](crate::Io::flush) and
    /// [`IoRef::write_ready`](crate::IoRef::write_ready). A wait that does not
    /// complete in time fails with [`io::ErrorKind::TimedOut`](std::io::ErrorKind::TimedOut),
    /// and the connection is left open for the caller to close. The polling
    /// methods, such as [`Io::poll_flush`](crate::Io::poll_flush), are not
    /// bounded.
    ///
    /// A zero duration disables the timeout. It is disabled by default, so
    /// a peer that does not read can pin the connection and its buffered
    /// output. Servers that accept untrusted peers should set it.
    #[must_use]
    pub fn set_write_timeout(mut self, timeout: Seconds) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Sets read-buffer watermarks.
    ///
    /// `high_watermark` enables read backpressure when the application-facing
    /// buffer reaches this size. It also selects the page size of read
    /// buffers, the smallest [`BytePageSize`] that holds `high_watermark`
    /// bytes, so a buffer is at least 4 KiB. Buffered data is compacted into a
    /// buffer of that page size when free capacity runs low; larger data grows
    /// the buffer by doubling its capacity, through larger page sizes. It must
    /// be greater than zero.
    /// `low_watermark` is the free-capacity threshold below which a read
    /// buffer is compacted or grown.
    ///
    /// Read buffers come from the per-thread page cache of `ntex-bytes`,
    /// shared with write buffers and all configurations, see
    /// [`ntex_bytes::set_page_cache_size`]. A buffer returns to the cache of
    /// the thread that drops its last reference. Buffers grown beyond the
    /// largest page size are freed. A connection holding
    /// unconsumed input, such as the start of a frame that has not fully
    /// arrived, keeps its whole read buffer, so each such connection uses at
    /// least one page until the rest arrives. Read-rate timeouts,
    /// see [`set_frame_read_rate`](Self::set_frame_read_rate), bound how long
    /// a slow peer can hold it.
    ///
    /// Frames that a codec splits off the read buffer, such as `Bytes`
    /// payloads, share its allocation. A frame kept alive keeps the whole
    /// read buffer allocated, it returns to the cache only once the last such
    /// frame is dropped, so retaining many small frames can use far more
    /// memory than their size.
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
    /// which are sized by [`set_write_page_size`](Self::set_write_page_size).
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
    /// Returns the page size of buffers acquired with [`get`](Self::get).
    ///
    /// This is the smallest [`BytePageSize`] that holds `high` bytes, or
    /// [`BytePageSize::Unset`] if `high` is larger than the largest page size.
    pub fn page_size(&self) -> BytePageSize {
        BytePageSize::for_capacity(self.high)
    }

    #[inline]
    /// Capacity of a buffer acquired with [`get`](Self::get).
    fn page_capacity(&self) -> usize {
        match self.page_size() {
            BytePageSize::Unset => self.high,
            size => size.capacity(),
        }
    }

    #[inline]
    /// Acquires an empty buffer from the thread-local page cache.
    ///
    /// The buffer has the page size returned by [`page_size`](Self::page_size),
    /// it returns to the page cache once dropped. If `high` is larger than the
    /// largest page size, a buffer with capacity `high` is allocated instead,
    /// it is freed once dropped.
    pub fn get(&self) -> BytesMut {
        match self.page_size() {
            BytePageSize::Unset => BytesMut::with_capacity(self.high),
            size => BytesMut::with_page_size(size),
        }
    }

    /// Creates a new uncached buffer with the specified capacity.
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity(cap)
    }

    #[inline]
    /// Makes room for another read once free capacity falls below `low`.
    ///
    /// When the buffered data plus `low` fits into a buffer acquired with
    /// [`get`](Self::get), the data is compacted into such a buffer, so the
    /// free capacity afterwards is its capacity minus the buffered length.
    /// Only larger data grows the buffer, in which case at least `high` bytes
    /// are free afterwards.
    pub fn resize(&self, buf: &mut BytesMut) {
        if buf.remaining_mut() < self.low {
            if buf.len() + self.low <= self.page_capacity() {
                self.resize_min(buf, self.low);
            } else {
                self.resize_min(buf, self.high);
            }
        }
    }

    #[inline]
    /// Ensures that the buffer has at least `size` bytes of remaining capacity.
    ///
    /// When the buffered data plus `size` fits into a buffer acquired with
    /// [`get`](Self::get), a buffer of that page size that is not shared with
    /// split-off data is compacted in place. Any other buffer is copied into a
    /// new buffer from [`get`](Self::get), and the old one returns to the page
    /// cache once its split-off data is dropped.
    ///
    /// Otherwise the buffer grows to double its capacity, or to enough
    /// capacity for `size` more bytes if that is larger. A pooled buffer moves
    /// to the page size that holds the new capacity. Beyond the largest page
    /// size, the buffer grows by at most 1 MiB at once and is not cached; a
    /// buffer that is not shared with split-off data is compacted in place
    /// when its allocation is large enough, or is reallocated, often without
    /// copying.
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
            let len = buf.len();
            if len + size <= self.page_capacity() {
                let page = self.page_size();
                if page != BytePageSize::Unset && buf.page_size() == page {
                    // compacts a unique page in place, copies a shared one
                    // into a new page
                    buf.reserve_exact(size);
                } else {
                    let mut new_buf = self.get();
                    new_buf.extend_from_slice(buf);
                    *buf = new_buf;
                }
                return;
            }

            let cap = buf.capacity();
            let new_cap = (len + size).max(cap + cap.min(MAX_GROW_STEP));
            buf.reserve_exact(new_cap - len);
        }
    }

    #[inline]
    /// Releases a buffer that is no longer used.
    ///
    /// The buffer is dropped. A pooled buffer returns to the page cache once
    /// split-off data that shares its allocation is dropped as well, other
    /// buffers are freed.
    pub fn release(&self, buf: BytesMut) {
        drop(buf);
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::cfg::SharedCfg;

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
    fn config_accessors() {
        let cfg = IoConfig::new();
        assert_eq!(cfg.connect_timeout(), Millis::ZERO);
        assert_eq!(cfg.keepalive_timeout(), Seconds(0));
        assert_eq!(cfg.write_page_size(), BytePageSize::Size16);

        let cfg = cfg
            .set_connect_timeout(Millis(500))
            .set_keepalive_timeout(Seconds(7))
            .set_write_page_size(BytePageSize::Size4);
        assert_eq!(cfg.connect_timeout(), Millis(500));
        assert_eq!(cfg.keepalive_timeout(), Seconds(7));
        assert_eq!(cfg.write_page_size(), BytePageSize::Size4);

        let shared = SharedCfg::new("CFG-TAG").add(cfg).build();
        let cfg = shared.get::<IoConfig>();
        assert_eq!(cfg.tag(), "CFG-TAG");
        assert_eq!(cfg.keepalive_timeout(), Seconds(7));

        // uncached buffers have exactly the requested capacity
        let buf = cfg.read_buf().buf_with_capacity(10);
        assert!(buf.is_empty());
        assert!(buf.capacity() >= 10);
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
    fn read_buffers_use_page_sizes() {
        // the default high watermark is a whole 16 KiB page
        let cfg = *IoConfig::new().read_buf();
        assert_eq!(cfg.page_size(), BytePageSize::Size16);
        let buf = cfg.get();
        assert_eq!(buf.page_size(), BytePageSize::Size16);
        assert_eq!(buf.capacity(), DEFAULT_HIGH);

        // the high watermark is rounded up to a page size, at least 4 KiB
        let cfg = *IoConfig::new().set_read_buf(8, 4).read_buf();
        assert_eq!(cfg.page_size(), BytePageSize::Size4);
        assert_eq!(cfg.get().capacity(), BytePageSize::Size4.capacity());
        assert_eq!((cfg.high, cfg.half), (8, 4));

        let cfg = *IoConfig::new().set_read_buf(5000, 512).read_buf();
        assert_eq!(cfg.page_size(), BytePageSize::Size8);
        assert_eq!(cfg.get().capacity(), BytePageSize::Size8.capacity());

        // above the largest page size buffers are not pooled
        let cfg = *IoConfig::new().set_read_buf(1024 * 1024, 1024).read_buf();
        assert_eq!(cfg.page_size(), BytePageSize::Unset);
        let buf = cfg.get();
        assert_eq!(buf.page_size(), BytePageSize::Unset);
        assert_eq!(buf.capacity(), 1024 * 1024);
    }

    #[test]
    fn released_pages_are_reused_by_configs() {
        let a = *IoConfig::new().read_buf();
        let b = *IoConfig::new().set_read_buf(DEFAULT_HIGH, 1024).read_buf();

        let buf = a.get();
        let ptr = buf.as_ptr();
        a.release(buf);
        let buf = b.get();
        assert_eq!(buf.as_ptr(), ptr, "page released by another config");

        // a config with another page size uses its own pages
        let small = *IoConfig::new().set_read_buf(1024, 256).read_buf();
        b.release(buf);
        let buf = small.get();
        assert_ne!(buf.as_ptr(), ptr);
        assert_eq!(buf.page_size(), BytePageSize::Size4);
        assert_eq!(b.get().as_ptr(), ptr);
        drop(buf);
    }

    #[test]
    fn resize_compacts_into_page() {
        let cfg = *IoConfig::new().set_read_buf(4096, 512).read_buf();
        let cap = cfg.get().capacity();
        assert_eq!(cap, BytePageSize::Size8.capacity());

        // leftover input at the end of a consumed buffer
        let mut buf = cfg.get();
        let ptr = buf.as_ptr();
        buf.extend_from_slice(&vec![1; cap - 100]);
        drop(buf.split_to(cap - 200));
        assert!(buf.remaining_mut() < cfg.low);

        // the page is not shared, the data moves to its start
        cfg.resize(&mut buf);
        assert_eq!(&buf[..], &[1; 100][..]);
        assert_eq!(buf.as_ptr(), ptr);
        assert_eq!(buf.capacity(), cap);
        assert_eq!(buf.remaining_mut(), cap - 100);

        // an explicit minimum that fits is compacted too
        let mut buf = cfg.get();
        buf.extend_from_slice(&vec![2; 6000]);
        drop(buf.split_to(3000));
        cfg.resize_min(&mut buf, 5000);
        assert_eq!(buf.capacity(), cap);
        assert_eq!(&buf[..], &[2; 3000][..]);

        // data that does not fit grows the buffer
        let mut buf = cfg.get();
        buf.extend_from_slice(&vec![3; cap - 100]);
        cfg.resize(&mut buf);
        assert_eq!(buf.len(), cap - 100);
        assert!(buf.remaining_mut() >= cfg.high);
        assert_eq!(buf.page_size(), BytePageSize::Size16);
    }

    #[test]
    fn resize_moves_shared_data_to_new_page() {
        let cfg = *IoConfig::new().read_buf();

        // a decoded frame still refers to the start of the buffer
        let mut buf = cfg.get();
        let ptr = buf.as_ptr();
        buf.extend_from_slice(&vec![1; cfg.high - 100]);
        let frame = buf.split_to(cfg.high - 1100);
        cfg.resize(&mut buf);
        assert_ne!(buf.as_ptr(), ptr);
        assert_eq!(buf.page_size(), BytePageSize::Size16);
        assert_eq!(&buf[..], &[1; 1000][..]);
        assert_eq!(buf.remaining_mut(), cfg.high - 1000);

        // the old page returns to the cache with the frame
        drop(frame);
        assert_eq!(cfg.get().as_ptr(), ptr);
    }

    #[test]
    fn resize_moves_other_buffers_to_page() {
        let cfg = *IoConfig::new().read_buf();

        // a buffer without a page size
        let mut buf = BytesMut::from(&b"input"[..]);
        cfg.resize(&mut buf);
        assert_eq!(&buf[..], b"input");
        assert_eq!(buf.page_size(), BytePageSize::Size16);
        assert_eq!(buf.capacity(), cfg.high);

        // a grown buffer shrinks once most of its data is consumed
        let mut buf = BytesMut::with_page_size(BytePageSize::Size64);
        let cap = buf.capacity();
        buf.extend_from_slice(&vec![2; cap]);
        drop(buf.split_to(cap - 100));
        cfg.resize(&mut buf);
        assert_eq!(&buf[..], &[2; 100][..]);
        assert_eq!(buf.page_size(), BytePageSize::Size16);
    }

    #[test]
    fn large_buffers_grow_through_page_sizes() {
        let cfg = *IoConfig::new().read_buf();

        // reads that fill the buffer until a 1 MiB frame is buffered
        let mut buf = cfg.get();
        let mut sizes = vec![buf.page_size()];
        let mut grows = 0;
        while buf.len() < 1024 * 1024 {
            let (ptr, cap) = (buf.as_ptr(), buf.capacity());
            cfg.resize(&mut buf);
            if buf.as_ptr() != ptr {
                grows += 1;
                assert!(buf.capacity() >= 2 * cap, "{cap} -> {}", buf.capacity());
                sizes.push(buf.page_size());
            }
            let n = buf.remaining_mut();
            buf.extend_from_slice(&vec![1; n]);
        }
        assert!(grows <= 8, "{grows} reallocations");
        assert!(buf.capacity() <= 2 * 1024 * 1024 + cfg.high);
        assert_eq!(
            &sizes[..6],
            &[
                BytePageSize::Size16,
                BytePageSize::Size32,
                BytePageSize::Size64,
                BytePageSize::Size128,
                BytePageSize::Size256,
                BytePageSize::Unset
            ]
        );

        // the step is limited for very large buffers
        let mut big = BytesMut::with_capacity(4 * MAX_GROW_STEP);
        big.extend_from_slice(&vec![2; 4 * MAX_GROW_STEP]);
        cfg.resize_min(&mut big, 1);
        assert_eq!(big.capacity(), 5 * MAX_GROW_STEP);
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
    fn shared_page_returns_after_frames_are_dropped() {
        let cfg = *IoConfig::new().read_buf();

        // a decoded frame still refers to the start of the buffer
        let mut buf = cfg.get();
        let ptr = buf.as_ptr();
        buf.extend_from_slice(&vec![1; cfg.high - 1000]);
        let frame = buf.split_to(buf.len());
        cfg.release(buf);
        let other = cfg.get();
        assert_ne!(other.as_ptr(), ptr);

        // once the frame is gone the page is reused
        drop(frame);
        assert_eq!(cfg.get().as_ptr(), ptr);
        drop(other);
    }

    #[test]
    #[should_panic(expected = "shutdown timeout must be greater than zero")]
    fn zero_shutdown_timeout_is_rejected() {
        let _ = IoConfig::new().set_shutdown_timeout(Seconds::ZERO);
    }
}
