use std::{cell::Cell, ptr::copy_nonoverlapping, rc::Rc, time};

use crate::http::{Request, Response};
use crate::io::{IoRef, Timer};
use crate::service::boxed::BoxService;
use crate::time::{sleep, Millis, Seconds, Sleep};
use crate::util::{BytesMut, PoolId};

#[derive(Debug, PartialEq, Clone, Copy)]
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
    pub(super) timer_h1: Timer,
    pub(super) pool: PoolId,
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
            Millis::ZERO,
            Seconds::ZERO,
            Millis(5_000),
            PoolId::P1,
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
        pool: PoolId,
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
            pool,
            timer: DateService::new(),
            timer_h1: Timer::default(),
        }))
    }
}

pub(super) type OnRequest = BoxService<(Request, IoRef), Request, Response>;

pub(super) struct DispatcherConfig<S, X, U> {
    pub(super) service: S,
    pub(super) expect: X,
    pub(super) upgrade: Option<U>,
    pub(super) keep_alive: Millis,
    pub(super) client_timeout: Millis,
    pub(super) client_disconnect: Seconds,
    pub(super) ka_enabled: bool,
    pub(super) timer: DateService,
    pub(super) timer_h1: Timer,
    pub(super) pool: PoolId,
    pub(super) on_request: Option<OnRequest>,
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
            service,
            expect,
            upgrade,
            on_request,
            keep_alive: cfg.0.keep_alive,
            client_timeout: cfg.0.client_timeout,
            client_disconnect: cfg.0.client_disconnect,
            ka_enabled: cfg.0.ka_enabled,
            timer: cfg.0.timer.clone(),
            timer_h1: cfg.0.timer_h1.clone(),
            pool: cfg.0.pool,
        }
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive_enabled(&self) -> bool {
        self.ka_enabled
    }

    /// Return keep-alive timer Sleep is configured.
    pub(super) fn keep_alive_timer(&self) -> Option<Sleep> {
        self.keep_alive.map(sleep)
    }

    /// Keep-alive expire time
    pub(super) fn keep_alive_expire(&self) -> Option<time::Instant> {
        self.keep_alive
            .map(|t| self.timer.now() + time::Duration::from(t))
    }
}

const DATE_VALUE_LENGTH_HDR: usize = 39;
const DATE_VALUE_DEFAULT: [u8; DATE_VALUE_LENGTH_HDR] = [
    b'd', b'a', b't', b'e', b':', b' ', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'0',
    b'0', b'0', b'0', b'0', b'0', b'0', b'0', b'\r', b'\n', b'\r', b'\n',
];

#[derive(Clone)]
pub struct DateService(Rc<DateServiceInner>);

impl Default for DateService {
    fn default() -> Self {
        DateService(Rc::new(DateServiceInner::new()))
    }
}

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

    pub(super) fn now(&self) -> time::Instant {
        self.check_date();
        self.0.current_time.get()
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
