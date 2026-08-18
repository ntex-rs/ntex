use std::{cell::Cell, cell::RefCell, rc::Rc, time};

use crate::io::{IoRef, cfg::FrameReadRate};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::{Millis, Seconds, sleep};
use crate::{channel::oneshot, util::BytePages, util::BytesMut, util::HashSet};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
/// Server keep-alive setting
pub enum KeepAlive {
    /// Keep alive in seconds
    Timeout(Seconds),
    /// Relay on OS to shutdown tcp connection
    Os,
    /// Disabled
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
/// Http service configuration
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
    /// Create instance of `HttpServiceConfig`.
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
    /// Set the maximum number of headers.
    ///
    /// When a request is received, the parser will reserve a buffer
    /// to store headers for optimal performance.
    ///
    /// If server receives more headers than the buffer size, it responds
    /// to the client with “431 Request Header Fields Too Large”.
    ///
    /// Default is set to 96
    pub fn set_max_headers(mut self, val: usize) -> Self {
        self.max_headers = val;
        self
    }

    #[must_use]
    /// Set the maximum buffer size for parsing http message.
    ///
    /// Default is 64kb
    pub fn set_max_buf_size(mut self, val: usize) -> Self {
        self.max_buf_size = val;
        self
    }

    #[must_use]
    /// Set server keep-alive setting.
    ///
    /// By default keep alive is set to a 5 seconds.
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
    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 5 seconds.
    pub fn set_keepalive_timeout(mut self, timeout: Seconds) -> Self {
        self.keep_alive = timeout;
        self.ka_enabled = !timeout.is_zero();
        self
    }

    #[must_use]
    /// Set request headers read timeout.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn set_client_timeout(mut self, timeout: Seconds) -> Self {
        if timeout.is_zero() {
            self.headers_read_rate = None;
        } else {
            let mut rate = self.headers_read_rate.unwrap_or(FrameReadRate {
                rate: 256,
                timeout: Seconds(5),
                max_timeout: Seconds(15),
            });
            rate.timeout = timeout;
            self.headers_read_rate = Some(rate);
        }
        self
    }

    #[must_use]
    /// Enable storing the headers in vector.
    ///
    /// By default, headers are not stored in vector.
    pub fn set_enable_headers_vec(mut self) -> Self {
        self.headers_vec = true;
        self
    }

    #[must_use]
    /// Set read rate parameters for request headers.
    ///
    /// Set read timeout, max timeout and rate for reading request headers. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default headers read rate is set to 1sec with max timeout 5sec.
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
    /// Set read rate parameters for request's payload.
    ///
    /// Set read timeout, max timeout and rate for reading payload. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default payload read rate is disabled.
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
