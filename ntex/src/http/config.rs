use std::{cell::Cell, ptr::copy_nonoverlapping, rc::Rc, time, time::Duration};

use ntex_h2::{self as h2};

use crate::http::{Request, Response};
use crate::service::{boxed::BoxService, Container};
use crate::time::{sleep, Millis, Seconds};
use crate::{io::IoRef, util::BytesMut};

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

/// Http service configuration
pub struct ServiceConfig(pub(super) Rc<Inner>);

pub(super) struct Inner {
    pub(super) keep_alive: Millis,
    pub(super) client_timeout: Millis,
    pub(super) client_disconnect: Seconds,
    pub(super) ka_enabled: bool,
    pub(super) timer: DateService,
    pub(super) ssl_handshake_timeout: Millis,
    pub(super) h2config: h2::Config,
}

impl Clone for ServiceConfig {
    fn clone(&self) -> Self {
        ServiceConfig(self.0.clone())
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self::new(
            KeepAlive::Timeout(Seconds(5)),
            Millis(1_000),
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
        client_timeout: Millis,
        client_disconnect: Seconds,
        ssl_handshake_timeout: Millis,
        h2config: h2::Config,
    ) -> ServiceConfig {
        let (keep_alive, ka_enabled) = match keep_alive {
            KeepAlive::Timeout(val) => (Millis::from(val), true),
            KeepAlive::Os => (Millis::ZERO, true),
            KeepAlive::Disabled => (Millis::ZERO, false),
        };
        let keep_alive = if ka_enabled { keep_alive } else { Millis::ZERO };

        ServiceConfig(Rc::new(Inner {
            keep_alive,
            ka_enabled,
            client_timeout,
            client_disconnect,
            ssl_handshake_timeout,
            h2config,
            timer: DateService::new(),
        }))
    }
}

pub(super) type OnRequest = BoxService<(Request, IoRef), Request, Response>;

pub(super) struct DispatcherConfig<S, X, U> {
    pub(super) service: Container<S>,
    pub(super) expect: Container<X>,
    pub(super) upgrade: Option<Container<U>>,
    pub(super) keep_alive: Duration,
    pub(super) client_timeout: Duration,
    pub(super) client_disconnect: Seconds,
    pub(super) h2config: h2::Config,
    pub(super) ka_enabled: bool,
    pub(super) timer: DateService,
    pub(super) on_request: Option<Container<OnRequest>>,
}

impl<S, X, U> DispatcherConfig<S, X, U> {
    pub(super) fn new(
        cfg: ServiceConfig,
        service: S,
        expect: X,
        upgrade: Option<U>,
        on_request: Option<OnRequest>,
    ) -> Self {
        DispatcherConfig {
            service: service.into(),
            expect: expect.into(),
            upgrade: upgrade.map(|v| v.into()),
            on_request: on_request.map(|v| v.into()),
            keep_alive: Duration::from(cfg.0.keep_alive),
            client_timeout: Duration::from(cfg.0.client_timeout),
            client_disconnect: cfg.0.client_disconnect,
            ka_enabled: cfg.0.ka_enabled,
            h2config: cfg.0.h2config.clone(),
            timer: cfg.0.timer.clone(),
        }
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive_enabled(&self) -> bool {
        self.ka_enabled
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
