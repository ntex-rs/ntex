use std::{cell::Cell, ptr::copy_nonoverlapping, rc::Rc, time::Duration};

use bytes::BytesMut;
use futures::{future, FutureExt};
use time::OffsetDateTime;

use crate::framed::Timer;
use crate::rt::time::{sleep, sleep_until, Instant, Sleep};

#[derive(Debug, PartialEq, Clone, Copy)]
/// Server keep-alive setting
pub enum KeepAlive {
    /// Keep alive in seconds
    Timeout(usize),
    /// Relay on OS to shutdown tcp connection
    Os,
    /// Disabled
    Disabled,
}

impl From<usize> for KeepAlive {
    fn from(keepalive: usize) -> Self {
        KeepAlive::Timeout(keepalive)
    }
}

impl From<Option<usize>> for KeepAlive {
    fn from(keepalive: Option<usize>) -> Self {
        if let Some(keepalive) = keepalive {
            KeepAlive::Timeout(keepalive)
        } else {
            KeepAlive::Disabled
        }
    }
}

/// Http service configuration
pub struct ServiceConfig(pub(super) Rc<Inner>);

pub(super) struct Inner {
    pub(super) keep_alive: u64,
    pub(super) client_timeout: u64,
    pub(super) client_disconnect: u64,
    pub(super) ka_enabled: bool,
    pub(super) timer: DateService,
    pub(super) ssl_handshake_timeout: u64,
    pub(super) timer_h1: Timer,
}

impl Clone for ServiceConfig {
    fn clone(&self) -> Self {
        ServiceConfig(self.0.clone())
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self::new(KeepAlive::Timeout(5), 0, 0, 5000)
    }
}

impl ServiceConfig {
    /// Create instance of `ServiceConfig`
    pub fn new(
        keep_alive: KeepAlive,
        client_timeout: u64,
        client_disconnect: u64,
        ssl_handshake_timeout: u64,
    ) -> ServiceConfig {
        let (keep_alive, ka_enabled) = match keep_alive {
            KeepAlive::Timeout(val) => (val as u64, true),
            KeepAlive::Os => (0, true),
            KeepAlive::Disabled => (0, false),
        };
        let keep_alive = if ka_enabled && keep_alive > 0 {
            keep_alive
        } else {
            0
        };

        ServiceConfig(Rc::new(Inner {
            keep_alive,
            ka_enabled,
            client_timeout,
            client_disconnect,
            ssl_handshake_timeout,
            timer: DateService::new(),
            timer_h1: Timer::default(),
        }))
    }
}

pub(super) struct DispatcherConfig<S, X, U> {
    pub(super) service: S,
    pub(super) expect: X,
    pub(super) upgrade: Option<U>,
    pub(super) keep_alive: Duration,
    pub(super) client_timeout: u64,
    pub(super) client_disconnect: u64,
    pub(super) ka_enabled: bool,
    pub(super) timer: DateService,
    pub(super) timer_h1: Timer,
}

impl<S, X, U> DispatcherConfig<S, X, U> {
    pub(super) fn new(
        cfg: ServiceConfig,
        service: S,
        expect: X,
        upgrade: Option<U>,
    ) -> Self {
        DispatcherConfig {
            service,
            expect,
            upgrade,
            keep_alive: Duration::from_secs(cfg.0.keep_alive),
            client_timeout: cfg.0.client_timeout,
            client_disconnect: cfg.0.client_disconnect,
            ka_enabled: cfg.0.ka_enabled,
            timer: cfg.0.timer.clone(),
            timer_h1: cfg.0.timer_h1.clone(),
        }
    }

    /// Return state of connection keep-alive functionality
    pub(super) fn keep_alive_enabled(&self) -> bool {
        self.ka_enabled
    }

    /// Return keep-alive timer Sleep is configured.
    pub(super) fn keep_alive_timer(&self) -> Option<Sleep> {
        if self.keep_alive.as_secs() != 0 {
            Some(sleep_until(self.timer.now() + self.keep_alive))
        } else {
            None
        }
    }

    /// Keep-alive expire time
    pub(super) fn keep_alive_expire(&self) -> Option<Instant> {
        if self.keep_alive.as_secs() != 0 {
            Some(self.timer.now() + self.keep_alive)
        } else {
            None
        }
    }

    pub(super) fn now(&self) -> Instant {
        self.timer.now()
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
    current_time: Cell<Instant>,
    current_date: Cell<[u8; DATE_VALUE_LENGTH_HDR]>,
}

impl DateServiceInner {
    fn new() -> Self {
        DateServiceInner {
            current: Cell::new(false),
            current_time: Cell::new(Instant::now()),
            current_date: Cell::new(DATE_VALUE_DEFAULT),
        }
    }

    fn update(&self) {
        self.current.set(true);
        self.current_time.set(Instant::now());

        let mut bytes = DATE_VALUE_DEFAULT;
        let dt = OffsetDateTime::now_utc().format("%a, %d %b %Y %H:%M:%S GMT");
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
            crate::rt::spawn(sleep(Duration::from_millis(500)).then(move |_| {
                s.0.current.set(false);
                future::ready(())
            }));
        }
    }

    fn now(&self) -> Instant {
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
        assert_eq!(KeepAlive::Timeout(10), Option::<usize>::Some(10).into());
    }
}
