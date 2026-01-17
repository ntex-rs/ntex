use std::{cell::Cell, ptr::copy_nonoverlapping, time};

use crate::service::cfg::{Cfg, CfgContext, Configuration};
use crate::time::{Millis, Seconds, sleep};
use crate::{io::cfg::FrameReadRate, service::Pipeline, util::BytesMut};

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

#[derive(Debug, Clone)]
/// Http service configuration
pub struct HttpServiceConfig {
    pub(super) keep_alive: Seconds,
    pub(super) ka_enabled: bool,
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
    /// Create instance of `HttpServiceConfig`
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
            payload_read_rate: None,
            config: CfgContext::default(),
        }
    }

    /// Set server keep-alive setting
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

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn set_keepalive_timeout(mut self, timeout: Seconds) -> Self {
        self.keep_alive = timeout;
        self.ka_enabled = !timeout.is_zero();
        self
    }

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
        if !timeout.is_zero() {
            self.headers_read_rate = Some(FrameReadRate {
                rate,
                timeout,
                max_timeout,
            });
        } else {
            self.headers_read_rate = None;
        }
        self
    }

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
        if !timeout.is_zero() {
            self.payload_read_rate = Some(FrameReadRate {
                rate,
                timeout,
                max_timeout,
            });
        } else {
            self.payload_read_rate = None;
        }
        self
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        /// Keep-alive enabled
        const KA_ENABLED = 0b0000_0001;
        /// Shutdown service
        const SHUTDOWN   = 0b0000_0010;
    }
}

pub(super) struct DispatcherConfig<S, C> {
    flags: Cell<Flags>,
    pub(super) config: &'static HttpServiceConfig,
    pub(super) service: Pipeline<S>,
    pub(super) control: Pipeline<C>,
}

impl<S, C> DispatcherConfig<S, C> {
    pub(super) fn new(config: Cfg<HttpServiceConfig>, service: S, control: C) -> Self {
        let config = config.into_static();
        DispatcherConfig {
            service: service.into(),
            control: control.into(),
            flags: Cell::new(if config.ka_enabled {
                Flags::KA_ENABLED
            } else {
                Flags::empty()
            }),
            config,
        }
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive(&self) -> Seconds {
        self.config.keep_alive
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive_enabled(&self) -> bool {
        self.flags.get().contains(Flags::KA_ENABLED)
    }

    pub(super) fn headers_read_rate(&self) -> Option<&FrameReadRate> {
        self.config.headers_read_rate.as_ref()
    }

    pub(super) fn payload_read_rate(&self) -> Option<&FrameReadRate> {
        self.config.payload_read_rate.as_ref()
    }

    /// Service is shuting down
    pub(super) fn is_shutdown(&self) -> bool {
        self.flags.get().contains(Flags::SHUTDOWN)
    }

    pub(super) fn shutdown(&self) {
        ntex_h2::ServiceConfig::shutdown();

        let mut flags = self.flags.get();
        flags.insert(Flags::SHUTDOWN);
        self.flags.set(flags);
    }
}

const DATE_VALUE_LENGTH_HDR: usize = 39;
const DATE_VALUE_DEFAULT: [u8; DATE_VALUE_LENGTH_HDR] = [
    b'd', b'a', b't', b'e', b':', b' ', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'\r', b'\n', b'\r', b'\n',
];

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
    fn check_date(&self) {
        DATE.with(|date| {
            if !date.current.get() {
                date.update();

                // periodic date update
                let _ = crate::rt::spawn(async move {
                    sleep(Millis(500)).await;
                    DATE.with(|date| {
                        date.current.set(false);
                    });
                });
            }
        })
    }

    pub(super) fn set_date<F: FnMut(&[u8])>(&self, mut f: F) {
        self.check_date();

        DATE.with(|date| {
            let date = date.current_date.get();
            f(&date[6..35])
        })
    }

    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        self.check_date();

        DATE.with(|date| {
            // SAFETY: reserves exact size
            let len = dst.len();
            dst.reserve(DATE_VALUE_LENGTH_HDR);
            unsafe {
                copy_nonoverlapping(
                    date.current_date.as_ptr().cast(),
                    dst.as_mut_ptr().add(len),
                    DATE_VALUE_LENGTH_HDR,
                );
                dst.set_len(len + DATE_VALUE_LENGTH_HDR)
            }
        })
    }

    #[doc(hidden)]
    pub fn bset_date_header(&self, dst: &mut BytesMut) {
        self.check_date();

        DATE.with(|date| {
            // SAFETY: reserves exact size
            let len = dst.len();
            dst.reserve(DATE_VALUE_LENGTH_HDR);
            unsafe {
                copy_nonoverlapping(
                    date.current_date.as_ptr().cast(),
                    dst.as_mut_ptr().add(len),
                    DATE_VALUE_LENGTH_HDR,
                );
                dst.set_len(len + DATE_VALUE_LENGTH_HDR)
            }
        })
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
