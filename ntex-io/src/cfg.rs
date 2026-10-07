//! I/O buffer, timeout, and frame-rate configuration.

use ntex_bytes::{BytePageSize, BytesMut};
use ntex_service::cfg::{CfgContext, Configuration};
use ntex_util::{time::Millis, time::Seconds};

#[derive(Debug)]
/// Shared configuration for an [`crate::Io`] stream.
pub struct IoConfig {
    connect_timeout: Millis,
    keepalive_timeout: Seconds,
    shutdown_timeout: Seconds,
    frame_read_rate: Option<FrameReadRate>,
    write_timeout: Seconds,

    // read side configuration
    read_size: BytePageSize,
    read_backpressure: usize,

    // write side configuration
    write_size: BytePageSize,
    write_backpressure: usize,
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

            read_size: BytePageSize::Size16,
            read_backpressure: BytePageSize::Size16.capacity(),

            write_timeout: Seconds(0),
            write_size: BytePageSize::Size16,
            write_backpressure: BytePageSize::Size16.capacity(),
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
    /// Returns the page size of read buffers.
    ///
    /// Read buffers are acquired from the `ntex-bytes` page cache with this
    /// page size, see [`set_read_size`](Self::set_read_size).
    pub fn read_size(&self) -> BytePageSize {
        self.read_size
    }

    #[inline]
    /// Returns the read backpressure high watermark.
    ///
    /// Read backpressure is enabled once buffered input reaches it and
    /// released once it falls to half of it, see
    /// [`set_read_backpressure`](Self::set_read_backpressure).
    pub fn read_backpressure(&self) -> usize {
        self.read_backpressure
    }

    #[inline]
    /// Returns the write backpressure timeout.
    ///
    /// A zero value means the timeout is disabled.
    pub fn write_timeout(&self) -> Seconds {
        self.write_timeout
    }

    #[inline]
    /// Returns the write backpressure high watermark.
    ///
    /// Write backpressure is released once outstanding output falls to half of
    /// it, see [`set_write_backpressure`](Self::set_write_backpressure).
    pub fn write_backpressure(&self) -> usize {
        self.write_backpressure
    }

    #[inline]
    /// Buffered input size that releases read backpressure.
    pub(crate) fn read_half(&self) -> usize {
        self.read_backpressure >> 1
    }

    #[inline]
    /// Outstanding output size that releases write backpressure.
    pub(crate) fn write_half(&self) -> usize {
        self.write_backpressure >> 1
    }

    #[inline]
    /// Acquires an empty read buffer from the thread-local page cache.
    pub(crate) fn new_read_buf(&self) -> BytesMut {
        BytesMut::with_page_size(self.read_size())
    }

    #[inline]
    /// Returns the write-buffer page size.
    pub fn write_size(&self) -> BytePageSize {
        self.write_size
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
    /// [write buffer](Self::set_write_backpressure) high watermark and disabled once the
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

    /// Sets the read-buffer page size.
    ///
    /// Read buffers are acquired with this page size. It also resets the read
    /// backpressure watermark to the page [`capacity`](BytePageSize::capacity),
    /// call [`set_read_backpressure`](Self::set_read_backpressure) afterwards
    /// to use another watermark. A buffer grows with [`BytesMut::reserve_more`] once less than
    /// [`BytePageSize::low`] of its page size remains: the data is compacted
    /// within its page when that leaves room for half a page, otherwise the
    /// buffer moves to the next page size.
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
    /// The default page size is 16 KiB.
    ///
    /// # Panics
    ///
    /// Panics if `size` is [`BytePageSize::Unset`].
    #[must_use]
    pub fn set_read_size(mut self, size: BytePageSize) -> Self {
        assert!(
            size != BytePageSize::Unset,
            "read buffer page size must be set"
        );
        self.read_size = size;
        self.read_backpressure = size.capacity();
        self
    }

    /// Sets the read backpressure watermark.
    ///
    /// Read backpressure is enabled when the application-facing read buffer
    /// reaches `size` bytes and released once it falls to half of it.
    ///
    /// [`set_read_size`](Self::set_read_size) resets the watermark to the
    /// read page capacity, approximately 16 KiB by default.
    ///
    /// # Panics
    ///
    /// Panics if `size` is zero.
    #[must_use]
    pub fn set_read_backpressure(mut self, size: usize) -> Self {
        assert!(size > 0, "read backpressure must be greater than zero");
        self.read_backpressure = size;
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
    /// [`set_write_backpressure`](Self::set_write_backpressure).
    ///
    /// The default page size is 16 KiB.
    ///
    /// # Panics
    ///
    /// Panics if `size` is [`BytePageSize::Unset`].
    #[must_use]
    pub fn set_write_size(mut self, size: BytePageSize) -> Self {
        assert!(
            size != BytePageSize::Unset,
            "write buffer page size must be set"
        );
        self.write_size = size;
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
    /// backpressure; use [`set_write_backpressure`](Self::set_write_backpressure) for
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
    /// Write backpressure is enabled at `size` bytes of outstanding output and
    /// must be greater than zero. Backpressure is released after the
    /// outstanding size falls to half of this value. Outstanding output is the
    /// buffered output plus any output a transport has taken ownership of but
    /// not yet written to the peer.
    ///
    /// Output is held in [`BytePages`](ntex_bytes::BytePages), which are sized
    /// by [`set_write_size`](Self::set_write_size).
    ///
    /// By default, the high watermark is approximately 16 KiB.
    ///
    /// # Panics
    ///
    /// Panics if `size` is zero.
    #[must_use]
    pub fn set_write_backpressure(mut self, size: usize) -> Self {
        assert!(size > 0, "write backpressure must be greater than zero");
        self.write_backpressure = size;
        self
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::cfg::SharedCfg;

    use super::*;

    #[test]
    fn buffer_configuration() {
        let cfg = IoConfig::new()
            .set_read_size(BytePageSize::Size4)
            .set_write_backpressure(2048);

        let size = BytePageSize::Size4;
        assert_eq!(cfg.read_backpressure(), size.capacity());
        assert_eq!(cfg.read_half(), size.capacity() / 2);
        assert_eq!(cfg.write_backpressure(), 2048);
        assert_eq!(cfg.write_half(), 1024);
    }

    #[test]
    fn read_size_configuration() {
        let default = BytePageSize::Size16.capacity();
        let cfg = IoConfig::new();
        assert_eq!(cfg.read_size(), BytePageSize::Size16);
        assert_eq!(cfg.read_backpressure(), default);
        assert_eq!(cfg.read_half(), default / 2);
        assert_eq!(cfg.write_backpressure(), default);
        let buf = cfg.new_read_buf();
        assert_eq!(buf.page_size(), BytePageSize::Size16);
        assert_eq!(buf.capacity(), default);

        let cfg = cfg
            .set_read_size(BytePageSize::Size4)
            .set_write_backpressure(2048);
        assert_eq!(cfg.read_size(), BytePageSize::Size4);
        assert_eq!(cfg.read_backpressure(), BytePageSize::Size4.capacity());
        assert_eq!(cfg.read_half(), BytePageSize::Size4.capacity() / 2);
        assert_eq!(cfg.new_read_buf().page_size(), BytePageSize::Size4);
        assert_eq!(cfg.write_backpressure(), 2048);
        assert_eq!(cfg.write_half(), 1024);

        let cfg = cfg.set_read_size(BytePageSize::Size256);
        assert_eq!(cfg.read_size(), BytePageSize::Size256);
        assert_eq!(cfg.read_backpressure(), BytePageSize::Size256.capacity());
        assert_eq!(cfg.new_read_buf().page_size(), BytePageSize::Size256);
    }

    #[test]
    fn read_backpressure_configuration() {
        let cfg = IoConfig::new()
            .set_read_size(BytePageSize::Size4)
            .set_read_backpressure(64 * 1024);
        assert_eq!(cfg.read_size(), BytePageSize::Size4);
        assert_eq!(cfg.read_backpressure(), 64 * 1024);
        assert_eq!(cfg.read_half(), 32 * 1024);
        assert_eq!(cfg.new_read_buf().page_size(), BytePageSize::Size4);

        // read size resets backpressure to the page capacity
        let cfg = cfg.set_read_size(BytePageSize::Size8);
        assert_eq!(cfg.read_backpressure(), BytePageSize::Size8.capacity());
    }

    #[test]
    #[should_panic(expected = "read buffer page size must be set")]
    fn unset_read_size() {
        let _ = IoConfig::new().set_read_size(BytePageSize::Unset);
    }

    #[test]
    #[should_panic(expected = "write buffer page size must be set")]
    fn unset_write_size() {
        let _ = IoConfig::new().set_write_size(BytePageSize::Unset);
    }

    #[test]
    #[should_panic(expected = "read backpressure must be greater than zero")]
    fn zero_read_backpressure() {
        let _ = IoConfig::new().set_read_backpressure(0);
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
        assert_eq!(cfg.write_size(), BytePageSize::Size16);

        let cfg = cfg
            .set_connect_timeout(Millis(500))
            .set_keepalive_timeout(Seconds(7))
            .set_write_size(BytePageSize::Size4);
        assert_eq!(cfg.connect_timeout(), Millis(500));
        assert_eq!(cfg.keepalive_timeout(), Seconds(7));
        assert_eq!(cfg.write_size(), BytePageSize::Size4);

        let shared = SharedCfg::new("CFG-TAG").add(cfg).build();
        let cfg = shared.get::<IoConfig>();
        assert_eq!(cfg.tag(), "CFG-TAG");
        assert_eq!(cfg.keepalive_timeout(), Seconds(7));
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
    #[should_panic(expected = "write backpressure must be greater than zero")]
    fn zero_write_backpressure() {
        let _ = IoConfig::new().set_write_backpressure(0);
    }

    #[test]
    #[should_panic(expected = "shutdown timeout must be greater than zero")]
    fn zero_shutdown_timeout_is_rejected() {
        let _ = IoConfig::new().set_shutdown_timeout(Seconds::ZERO);
    }
}
