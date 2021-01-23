use std::{
    cell::UnsafeCell, fmt, fmt::Write, ptr::copy_nonoverlapping, rc::Rc, time::Duration,
};

use bytes::BytesMut;
use futures::{future, FutureExt};
use time::OffsetDateTime;

use crate::framed::Timer;
use crate::rt::time::{delay_for, delay_until, Delay, Instant};

// "Sun, 06 Nov 1994 08:49:37 GMT".len()
const DATE_VALUE_LENGTH: usize = 29;

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
    pub(super) keep_alive: u64,
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
            keep_alive: cfg.0.keep_alive,
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

    /// Return keep-alive timer delay is configured.
    pub(super) fn keep_alive_timer(&self) -> Option<Delay> {
        if self.keep_alive != 0 {
            Some(delay_until(
                self.timer.now() + Duration::from_secs(self.keep_alive),
            ))
        } else {
            None
        }
    }

    /// Keep-alive expire time
    pub(super) fn keep_alive_expire(&self) -> Option<Instant> {
        if self.keep_alive != 0 {
            Some(self.timer.now() + Duration::from_secs(self.keep_alive))
        } else {
            None
        }
    }

    pub(super) fn now(&self) -> Instant {
        self.timer.now()
    }
}

#[derive(Copy, Clone)]
pub(super) struct Date {
    pub(super) bytes: [u8; DATE_VALUE_LENGTH],
    pos: usize,
}

impl Date {
    fn new() -> Date {
        let mut date = Date {
            bytes: [0; DATE_VALUE_LENGTH],
            pos: 0,
        };
        date.update();
        date
    }
    fn update(&mut self) {
        self.pos = 0;
        write!(
            self,
            "{}",
            OffsetDateTime::now_utc().format("%a, %d %b %Y %H:%M:%S GMT")
        )
        .unwrap();
    }
}

impl fmt::Write for Date {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let len = s.len();
        self.bytes[self.pos..self.pos + len].copy_from_slice(s.as_bytes());
        self.pos += len;
        Ok(())
    }
}

#[derive(Clone)]
pub struct DateService(Rc<DateServiceInner>);

impl Default for DateService {
    fn default() -> Self {
        DateService(Rc::new(DateServiceInner::new()))
    }
}

struct DateServiceInner {
    current: UnsafeCell<Option<(Date, Instant)>>,
}

impl DateServiceInner {
    fn new() -> Self {
        DateServiceInner {
            current: UnsafeCell::new(None),
        }
    }

    fn reset(&self) {
        unsafe { (&mut *self.current.get()).take() };
    }

    fn update(&self) {
        let now = Instant::now();
        let date = Date::new();
        *(unsafe { &mut *self.current.get() }) = Some((date, now));
    }
}

impl DateService {
    fn new() -> Self {
        DateService(Rc::new(DateServiceInner::new()))
    }

    fn check_date(&self) {
        if unsafe { (&*self.0.current.get()).is_none() } {
            self.0.update();

            // periodic date update
            let s = self.clone();
            crate::rt::spawn(delay_for(Duration::from_millis(500)).then(move |_| {
                s.0.reset();
                future::ready(())
            }));
        }
    }

    fn now(&self) -> Instant {
        self.check_date();
        unsafe { (&*self.0.current.get()).as_ref().unwrap().1 }
    }

    pub(super) fn set_date<F: FnMut(&Date)>(&self, mut f: F) {
        self.check_date();
        f(&unsafe { (&*self.0.current.get()).as_ref().unwrap().0 })
    }

    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        const HEAD: &[u8] = b"date: ";
        const TAIL: &[u8] = b"\r\n\r\n";
        // date bytes len
        const N: usize = 29;
        const TOTAL: usize = 39;

        dst.reserve(TOTAL);
        // SAFETY: previous reserve exact size
        unsafe {
            let buf = dst.as_mut_ptr().add(dst.len());
            copy_nonoverlapping(HEAD.as_ptr(), buf, HEAD.len());
            self.set_date(|date| {
                copy_nonoverlapping(date.bytes.as_ptr(), buf.add(HEAD.len()), N)
            });
            copy_nonoverlapping(TAIL.as_ptr(), buf.add(N + HEAD.len()), TAIL.len());
            dst.set_len(dst.len() + TOTAL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_len() {
        assert_eq!(DATE_VALUE_LENGTH, "Sun, 06 Nov 1994 08:49:37 GMT".len());
    }

    #[ntex_rt::test]
    async fn test_date() {
        let date = DateService::default();
        let mut buf1 = BytesMut::with_capacity(DATE_VALUE_LENGTH + 10);
        date.set_date_header(&mut buf1);
        let mut buf2 = BytesMut::with_capacity(DATE_VALUE_LENGTH + 10);
        date.set_date_header(&mut buf2);
        assert_eq!(buf1, buf2);
    }

    #[test]
    fn keep_alive() {
        assert_eq!(KeepAlive::Disabled, Option::<usize>::None.into());
        assert_eq!(KeepAlive::Timeout(10), Option::<usize>::Some(10).into());
    }
}
