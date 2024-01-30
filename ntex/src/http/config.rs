use std::{cell::Cell, ptr::copy_nonoverlapping, rc::Rc, time};

use ntex_h2::{self as h2};

use crate::time::{sleep, Millis, Seconds};
use crate::{service::Pipeline, util::BytesMut};

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
pub struct ServiceConfig {
    pub(super) keep_alive: Seconds,
    pub(super) client_disconnect: Seconds,
    pub(super) ka_enabled: bool,
    pub(super) ssl_handshake_timeout: Millis,
    pub(super) h2config: h2::Config,
    pub(super) headers_read_rate: Option<ReadRate>,
    pub(super) payload_read_rate: Option<ReadRate>,
    pub(super) timer: DateService,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ReadRate {
    pub(super) rate: u16,
    pub(super) timeout: Seconds,
    pub(super) max_timeout: Seconds,
}

impl Default for ReadRate {
    fn default() -> Self {
        ReadRate {
            rate: 256,
            timeout: Seconds(5),
            max_timeout: Seconds(15),
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self::new(
            KeepAlive::Timeout(Seconds(5)),
            Seconds::ONE,
            Seconds::ONE,
            Millis(5_000),
            h2::Config::server(),
        )
    }
}

impl ServiceConfig {
    /// Create instance of `ServiceConfig`
    pub fn new(
        keep_alive: KeepAlive,
        client_timeout: Seconds,
        client_disconnect: Seconds,
        ssl_handshake_timeout: Millis,
        h2config: h2::Config,
    ) -> ServiceConfig {
        let (keep_alive, ka_enabled) = match keep_alive {
            KeepAlive::Timeout(val) => (val, true),
            KeepAlive::Os => (Seconds::ZERO, true),
            KeepAlive::Disabled => (Seconds::ZERO, false),
        };
        let keep_alive = if ka_enabled {
            keep_alive
        } else {
            Seconds::ZERO
        };

        ServiceConfig {
            client_disconnect,
            ssl_handshake_timeout,
            h2config,
            keep_alive,
            ka_enabled,
            timer: DateService::new(),
            headers_read_rate: Some(ReadRate {
                rate: 256,
                timeout: client_timeout,
                max_timeout: client_timeout + Seconds(15),
            }),
            payload_read_rate: None,
        }
    }

    pub(crate) fn client_timeout(&mut self, timeout: Seconds) {
        if timeout.is_zero() {
            self.headers_read_rate = None;
        } else {
            let mut rate = self.headers_read_rate.unwrap_or_default();
            rate.timeout = timeout;
            self.headers_read_rate = Some(rate);
        }
    }

    /// Set server keep-alive setting
    ///
    /// By default keep alive is set to a 5 seconds.
    pub fn keepalive<W: Into<KeepAlive>>(&mut self, val: W) -> &mut Self {
        let (keep_alive, ka_enabled) = match val.into() {
            KeepAlive::Timeout(val) => (val, true),
            KeepAlive::Os => (Seconds::ZERO, true),
            KeepAlive::Disabled => (Seconds::ZERO, false),
        };
        let keep_alive = if ka_enabled {
            keep_alive
        } else {
            Seconds::ZERO
        };

        self.keep_alive = keep_alive;
        self.ka_enabled = ka_enabled;
        self
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.keep_alive = timeout;
        self.ka_enabled = !timeout.is_zero();
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
    pub fn disconnect_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.client_disconnect = timeout;
        self.h2config.disconnect_timeout(timeout);
        self
    }

    /// Set server ssl handshake timeout.
    ///
    /// Defines a timeout for connection ssl handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn ssl_handshake_timeout(&mut self, timeout: Seconds) -> &mut Self {
        self.ssl_handshake_timeout = timeout.into();
        self.h2config.handshake_timeout(timeout);
        self
    }

    /// Set read rate parameters for request headers.
    ///
    /// Set read timeout, max timeout and rate for reading request headers. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default headers read rate is set to 1sec with max timeout 5sec.
    pub fn headers_read_rate(
        &mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> &mut Self {
        if !timeout.is_zero() {
            self.headers_read_rate = Some(ReadRate {
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
    pub fn payload_read_rate(
        &mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> &mut Self {
        if !timeout.is_zero() {
            self.payload_read_rate = Some(ReadRate {
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

pub(super) struct DispatcherConfig<S, C> {
    pub(super) service: Pipeline<S>,
    pub(super) control: Pipeline<C>,
    pub(super) keep_alive: Seconds,
    pub(super) client_disconnect: Seconds,
    pub(super) h2config: h2::Config,
    pub(super) ka_enabled: bool,
    pub(super) headers_read_rate: Option<ReadRate>,
    pub(super) payload_read_rate: Option<ReadRate>,
    pub(super) timer: DateService,
}

impl<S, C> DispatcherConfig<S, C> {
    pub(super) fn new(cfg: ServiceConfig, service: S, control: C) -> Self {
        DispatcherConfig {
            service: service.into(),
            control: control.into(),
            keep_alive: cfg.keep_alive,
            client_disconnect: cfg.client_disconnect,
            ka_enabled: cfg.ka_enabled,
            headers_read_rate: cfg.headers_read_rate,
            payload_read_rate: cfg.payload_read_rate,
            h2config: cfg.h2config.clone(),
            timer: cfg.timer.clone(),
        }
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive_enabled(&self) -> bool {
        self.ka_enabled
    }

    pub(super) fn headers_read_rate(&self) -> Option<&ReadRate> {
        self.headers_read_rate.as_ref()
    }
}

const DATE_VALUE_LENGTH_HDR: usize = 39;
const DATE_VALUE_DEFAULT: [u8; DATE_VALUE_LENGTH_HDR] = [
    b'd', b'a', b't', b'e', b':', b' ', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'\r', b'\n', b'\r', b'\n',
];

#[derive(Debug, Clone)]
pub struct DateService(Rc<DateServiceInner>);

impl Default for DateService {
    fn default() -> Self {
        DateService(Rc::new(DateServiceInner::new()))
    }
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
    fn new() -> Self {
        DateService(Rc::new(DateServiceInner::new()))
    }

    fn check_date(&self) {
        if !self.0.current.get() {
            self.0.update();

            // periodic date update
            let s = self.clone();
            crate::rt::spawn(async move {
                sleep(Millis(500)).await;
                s.0.current.set(false);
            });
        }
    }

    pub(super) fn set_date<F: FnMut(&[u8])>(&self, mut f: F) {
        self.check_date();
        let date = self.0.current_date.get();
        f(&date[6..35])
    }

    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        self.check_date();

        // SAFETY: reserves exact size
        let len = dst.len();
        dst.reserve(DATE_VALUE_LENGTH_HDR);
        unsafe {
            copy_nonoverlapping(
                self.0.current_date.as_ptr().cast(),
                dst.as_mut_ptr().add(len),
                DATE_VALUE_LENGTH_HDR,
            );
            dst.set_len(len + DATE_VALUE_LENGTH_HDR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn test_date() {
        let date = DateService::default();
        let mut buf1 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        date.set_date_header(&mut buf1);
        let mut buf2 = BytesMut::with_capacity(DATE_VALUE_LENGTH_HDR);
        date.set_date_header(&mut buf2);
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
