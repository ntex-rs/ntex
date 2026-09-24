use std::{cell::Cell, cell::RefCell, rc::Rc, time};

use crate::io::{IoRef, cfg::FrameReadRate};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::{Millis, Seconds, sleep};
use crate::{channel::oneshot, util::BytePages, util::BytesMut, util::HashSet};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
/// Server keep-alive behavior.
pub enum KeepAlive {
    /// Close an idle connection after this timeout.
    Timeout(Seconds),
    /// Keep the connection open until the peer or operating system closes it.
    Os,
    /// Disable persistent connections.
    Disabled,
}

impl From<usize> for KeepAlive {
    fn from(keepalive: usize) -> Self {
        KeepAlive::Timeout(Seconds(keepalive as u16))
    }
}

impl From<Seconds> for KeepAlive {
    fn from(keepalive: Seconds) -> Self {
        KeepAlive::Timeout(keepalive)
    }
}

impl From<Option<usize>> for KeepAlive {
    fn from(keepalive: Option<usize>) -> Self {
        if let Some(keepalive) = keepalive {
            KeepAlive::Timeout(Seconds(keepalive as u16))
        } else {
            KeepAlive::Disabled
        }
    }
}

#[derive(Debug)]
/// Configuration shared by HTTP/1 and HTTP/2 server services.
///
/// The default configuration enables persistent HTTP/1 connections with a
/// five-second idle timeout, allows 96 headers, limits the message-head buffer
/// to 64 KiB, and applies a one-second initial request-header timeout.
pub struct HttpServiceConfig {
    pub(super) keep_alive: Seconds,
    pub(super) ka_enabled: bool,
    pub(super) headers_vec: bool,
    pub(super) max_headers: usize,
    pub(super) max_buf_size: usize,
    pub(super) headers_read_rate: Option<FrameReadRate>,
    pub(super) payload_read_rate: Option<FrameReadRate>,

    config: CfgContext,
}

impl Default for HttpServiceConfig {
    fn default() -> Self {
        HttpServiceConfig::new()
    }
}

impl Configuration for HttpServiceConfig {
    const NAME: &str = "Http service configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl HttpServiceConfig {
    #[must_use]
    /// Creates an HTTP service configuration with default settings.
    pub fn new() -> HttpServiceConfig {
        Self::_new(KeepAlive::Timeout(Seconds(5)), Seconds::ONE)
    }

    fn _new(keep_alive: KeepAlive, client_timeout: Seconds) -> HttpServiceConfig {
        let (keep_alive, ka_enabled) = match keep_alive {
            KeepAlive::Timeout(val) => (val, true),
            KeepAlive::Os => (Seconds::ZERO, true),
            KeepAlive::Disabled => (Seconds::ZERO, false),
        };
        let keep_alive = if ka_enabled { keep_alive } else { Seconds::ZERO };

        HttpServiceConfig {
            keep_alive,
            ka_enabled,
            headers_read_rate: Some(FrameReadRate {
                rate: 256,
                timeout: client_timeout,
                max_timeout: client_timeout + Seconds(15),
            }),
            max_headers: 96,
            max_buf_size: 64 * 1024,
            headers_vec: false,
            payload_read_rate: None,
            config: CfgContext::default(),
        }
    }

    #[must_use]
    /// Sets the maximum number of headers in a message.
    ///
    /// Requests exceeding this limit are rejected with
    /// `431 Request Header Fields Too Large`. The default is 96.
    pub fn set_max_headers(mut self, val: usize) -> Self {
        self.max_headers = val;
        self
    }

    #[must_use]
    /// Sets the maximum cumulative size of an HTTP message head.
    ///
    /// The request or response line, headers, and terminating empty line may
    /// occupy up to and including this number of bytes. Larger message heads
    /// are rejected. The default is 64 KiB.
    pub fn set_max_buf_size(mut self, val: usize) -> Self {
        self.max_buf_size = val;
        self
    }

    #[must_use]
    /// Sets the server keep-alive behavior.
    ///
    /// By default, idle persistent connections are closed after five seconds.
    pub fn set_keepalive<W: Into<KeepAlive>>(mut self, val: W) -> Self {
        let (keep_alive, ka_enabled) = match val.into() {
            KeepAlive::Timeout(val) => (val, true),
            KeepAlive::Os => (Seconds::ZERO, true),
            KeepAlive::Disabled => (Seconds::ZERO, false),
        };
        let keep_alive = if ka_enabled { keep_alive } else { Seconds::ZERO };

        self.keep_alive = keep_alive;
        self.ka_enabled = ka_enabled;
        self
    }

    #[must_use]
    /// Sets the keep-alive timeout.
    ///
    /// A zero duration disables persistent connections rather than selecting
    /// an unlimited timeout. Use [`KeepAlive::Os`] with
    /// [`set_keepalive`](Self::set_keepalive) to leave connection lifetime to
    /// the peer or operating system. The default is five seconds.
    pub fn set_keepalive_timeout(mut self, timeout: Seconds) -> Self {
        self.keep_alive = timeout;
        self.ka_enabled = !timeout.is_zero();
        self
    }

    #[must_use]
    /// Sets the initial timeout for reading request headers.
    ///
    /// If the client does not begin transmitting a complete header block
    /// within this period, the request is rejected with `408 Request Timeout`.
    /// A zero duration disables header-read timing, allowing a new connection
    /// to wait indefinitely for its first request independently of the
    /// keep-alive policy. The default is one second.
    ///
    /// This sets the measurement interval of the request-head read rate. The
    /// cumulative limit and required rate configured by
    /// [`set_headers_read_rate`](Self::set_headers_read_rate) are kept. If
    /// header-read timing was disabled, it is enabled again with the default
    /// rate of 256 bytes and a cumulative limit of `timeout` plus 15 seconds.
    pub fn set_client_timeout(mut self, timeout: Seconds) -> Self {
        if timeout.is_zero() {
            self.headers_read_rate = None;
        } else {
            let mut rate = self.headers_read_rate.unwrap_or(FrameReadRate {
                rate: 256,
                timeout,
                max_timeout: timeout + Seconds(15),
            });
            rate.timeout = timeout;
            self.headers_read_rate = Some(rate);
        }
        self
    }

    #[must_use]
    /// Preserves headers in their original order and casing.
    ///
    /// When enabled, decoded headers are additionally copied into
    /// [`RequestHead::headers_vec`](crate::http::RequestHead::headers_vec) or
    /// [`ResponseHead::headers_vec`](crate::http::ResponseHead::headers_vec).
    /// The normal header map remains populated. This is disabled by default.
    pub fn set_headers_vec(mut self, enabled: bool) -> Self {
        self.headers_vec = enabled;
        self
    }

    #[must_use]
    /// Sets read-rate limits for request headers.
    ///
    /// This setting protects HTTP/1 connections from clients that send a
    /// request line or headers too slowly. The timer starts when the connection
    /// begins waiting for the initial request. On a persistent connection, it
    /// starts again after bytes for the next request head arrive.
    ///
    /// `timeout` is the duration of one measurement interval. When an interval
    /// expires, the dispatcher grants another interval only if more than
    /// `rate` new bytes were received. The request head must complete before
    /// the cumulative `max_timeout` is exhausted. All newly received
    /// request-head bytes count toward progress, including request-line and
    /// header bytes that the incremental parser has already consumed.
    ///
    /// A zero `timeout` disables request-head timing. A zero `max_timeout`
    /// removes the cumulative limit, allowing the deadline to be extended
    /// indefinitely while the required read rate is maintained. When
    /// `max_timeout` is not an exact multiple of `timeout`, the final
    /// measurement interval is shortened so the cumulative limit is not
    /// exceeded.
    ///
    /// If the request head misses its deadline, the HTTP/1 control service
    /// receives
    /// [`ProtocolError::SlowRequestTimeout`](crate::http::h1::ProtocolError::SlowRequestTimeout).
    /// The default control service responds with `408 Request Timeout` and
    /// closes the connection.
    ///
    /// By default, the timeout is 1 second and the maximum timeout is 16
    /// seconds, with more than 256 bytes required for each extension.
    ///
    /// # Example
    ///
    /// ```rust
    /// use ntex::http::HttpServiceConfig;
    /// use ntex::time::Seconds;
    ///
    /// let config = HttpServiceConfig::new().set_headers_read_rate(
    ///     Seconds(2),  // measurement interval
    ///     Seconds(10), // maximum time for one request head
    ///     512,         // bytes required to extend the deadline for next 2 seconds
    /// );
    /// ```
    pub fn set_headers_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> Self {
        if timeout.is_zero() {
            self.headers_read_rate = None;
        } else {
            self.headers_read_rate = Some(FrameReadRate {
                rate,
                timeout,
                max_timeout,
            });
        }
        self
    }

    #[must_use]
    /// Sets read-rate limits for request payloads.
    ///
    /// This setting protects HTTP/1 connections from clients that send a
    /// request body too slowly. The timer starts when the dispatcher begins
    /// decoding a request payload. At the end of each `timeout`
    /// interval, another interval is granted only if more than `rate` bytes
    /// were decoded.
    ///
    /// The timer runs only while the dispatcher can read and forward payload
    /// data. It is paused while application payload backpressure or response
    /// write backpressure prevents further reads, so those conditions are not
    /// treated as a slow network peer. Pausing preserves the unused portion of
    /// the cumulative `max_timeout`; resuming does not grant a new maximum
    /// period. The timer stops when the complete payload has been decoded.
    ///
    /// A zero `timeout` disables payload timing. A zero `max_timeout` removes
    /// the cumulative limit, allowing the deadline to be extended indefinitely
    /// while the required read rate is maintained. When `max_timeout` is not
    /// an exact multiple of `timeout`, the final measurement interval is
    /// shortened so the cumulative limit is not exceeded.
    ///
    /// If the payload misses its deadline, its stream receives a timed-out
    /// [`PayloadError`](crate::http::error::PayloadError), and the HTTP/1
    /// control service receives
    /// [`ProtocolError::SlowPayloadTimeout`](crate::http::h1::ProtocolError::SlowPayloadTimeout).
    /// The default control service responds with `408 Request Timeout` and
    /// closes the connection.
    ///
    /// Payload read-rate limiting is disabled by default.
    ///
    /// # Example
    ///
    /// ```rust
    /// use ntex::http::HttpServiceConfig;
    /// use ntex::time::Seconds;
    ///
    /// let config = HttpServiceConfig::new().set_payload_read_rate(
    ///     Seconds(2),  // measurement interval
    ///     Seconds(30), // maximum time for one request payload
    ///     1024,        // bytes required to extend the deadline
    /// );
    /// ```
    pub fn set_payload_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> Self {
        if timeout.is_zero() {
            self.payload_read_rate = None;
        } else {
            self.payload_read_rate = Some(FrameReadRate {
                rate,
                timeout,
                max_timeout,
            });
        }
        self
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        /// Shutdown service
        const SHUTDOWN   = 0b0000_0010;
    }
}

#[derive(Clone)]
pub(super) struct DispatcherConfig(Rc<DispatcherConfigInner>);

struct DispatcherConfigInner {
    flags: Cell<Flags>,
    idx: Cell<usize>,
    pub(super) inflight: RefCell<HashSet<IoRef>>,
    rx: Cell<Option<oneshot::Receiver<()>>>,
    tx: Cell<Option<oneshot::Sender<()>>>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        let (tx, rx) = oneshot::channel();

        DispatcherConfig(Rc::new(DispatcherConfigInner {
            idx: Cell::new(0),
            flags: Cell::new(Flags::empty()),
            rx: Cell::new(Some(rx)),
            tx: Cell::new(Some(tx)),
            inflight: RefCell::new(HashSet::default()),
        }))
    }
}

impl DispatcherConfig {
    /// Get connection id
    pub(super) fn next_id(&self) -> usize {
        let id = self.0.idx.get();
        self.0.idx.set(id + 1);
        id
    }

    pub(super) fn remove_io(&self, io: &IoRef) -> usize {
        let mut inflight = self.0.inflight.borrow_mut();
        inflight.remove(io);
        inflight.len()
    }

    pub(super) fn insert_io(&self, io: &IoRef) -> usize {
        let mut inflight = self.0.inflight.borrow_mut();
        inflight.insert(io.clone());
        inflight.len()
    }

    /// Service is shutting down
    pub(super) fn is_shutdown(&self) -> bool {
        self.0.flags.get().contains(Flags::SHUTDOWN)
    }

    pub(super) fn shutdown(&self) -> usize {
        ntex_h2::ServiceConfig::shutdown();

        let mut flags = self.0.flags.get();
        flags.insert(Flags::SHUTDOWN);
        self.0.flags.set(flags);

        let inflight = self.0.inflight.borrow();
        for io in inflight.iter() {
            io.notify_dispatcher();
        }
        inflight.len()
    }

    pub(super) async fn wait_shutdown(&self) {
        if let Some(rx) = self.0.rx.take() {
            let _ = rx.await;
        }
    }

    pub(super) fn notify_shutdown(&self) {
        if let Some(tx) = self.0.tx.take() {
            let _ = tx.send(());
        }
    }
}

const DATE_VALUE_LENGTH_HDR: usize = 39;
const DATE_VALUE_DEFAULT: [u8; DATE_VALUE_LENGTH_HDR] =
    *b"date: 00000000000000000000000000000\r\n\r\n";

#[derive(Debug, Copy, Clone)]
/// Generates the cached HTTP `Date` header used by the protocol encoders.
///
/// The cached value is refreshed periodically and avoids formatting the
/// current system time for every response. Applications normally do not need
/// to use this type directly.
pub struct DateService;

thread_local! {
    static DATE: DateServiceInner = DateServiceInner::new();
}

#[derive(Debug)]
struct DateServiceInner {
    current: Cell<bool>,
    current_time: Cell<time::Instant>,
    current_date: Cell<[u8; DATE_VALUE_LENGTH_HDR]>,
}

impl DateServiceInner {
    fn new() -> Self {
        DateServiceInner {
            current: Cell::new(false),
            current_time: Cell::new(time::Instant::now()),
            current_date: Cell::new(DATE_VALUE_DEFAULT),
        }
    }

    fn update(&self) {
        self.current.set(true);
        self.current_time.set(time::Instant::now());

        let mut bytes = DATE_VALUE_DEFAULT;
        let dt = httpdate::HttpDate::from(time::SystemTime::now()).to_string();
        bytes[6..35].copy_from_slice(dt.as_ref());
        self.current_date.set(bytes);
    }
}

impl DateService {
    fn check_date() {
        DATE.with(|date| {
            if !date.current.get() {
                date.update();

                // periodic date update
                crate::rt::spawn(async move {
                    sleep(Millis(500)).await;
                    DATE.with(|date| {
                        date.current.set(false);
                    });
                });
            }
        });
    }

    pub(super) fn set_date<F: FnMut(&[u8])>(mut f: F) {
        DateService::check_date();
        DATE.with(|date| {
            let date = date.current_date.get();
            f(&date[6..35]);
        });
    }

    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        DateService::check_date();
        DATE.with(|date| {
            dst.extend_from_slice(unsafe { date.current_date.as_ptr().as_ref().unwrap() });
        });
    }

    #[doc(hidden)]
    pub fn set_date_header2(&self, dst: &mut BytePages) {
        DateService::check_date();
        DATE.with(|date| {
            dst.extend_from_slice(unsafe { date.current_date.as_ptr().as_ref().unwrap() });
        });
    }

    #[doc(hidden)]
    pub fn bset_date_header(&self, dst: &mut BytesMut) {
        DateService::check_date();
        DATE.with(|date| {
            dst.extend_from_slice(unsafe { date.current_date.as_ptr().as_ref().unwrap() });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn test_date() {
        let mut buf1 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        DateService.set_date_header(&mut buf1);
        let mut buf2 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        DateService.set_date_header(&mut buf2);
        assert_eq!(buf1, buf2);

        let mut buf1 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        DateService.bset_date_header(&mut buf1);
        let mut buf2 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        DateService.bset_date_header(&mut buf2);
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn keep_alive() {
        assert_eq!(KeepAlive::Disabled, Option::<usize>::None.into());
        assert_eq!(
            KeepAlive::Timeout(Seconds(10)),
            Option::<usize>::Some(10).into()
        );
    }
}
