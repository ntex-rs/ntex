use std::convert::TryInto;

/// A Duration type to represent a span of time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(pub(super) u64);

impl Duration {
    pub const ZERO: Duration = Duration(0);

    #[inline]
    pub const fn from_secs(secs: u32) -> Duration {
        Duration((secs as u64) * 1000)
    }

    #[inline]
    pub const fn from_millis(millis: u64) -> Duration {
        Duration(millis)
    }

    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Call function `f` if duration is none zero.
    #[inline]
    pub fn map<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(Duration) -> R,
    {
        if self.0 == 0 {
            None
        } else {
            Some(f(*self))
        }
    }
}

impl From<u64> for Duration {
    #[inline]
    fn from(millis: u64) -> Duration {
        Duration(millis)
    }
}

impl From<Seconds> for Duration {
    #[inline]
    fn from(s: Seconds) -> Duration {
        Duration((s.0 as u64) * 1000)
    }
}

impl From<std::time::Duration> for Duration {
    #[inline]
    fn from(d: std::time::Duration) -> Duration {
        Self(d.as_millis().try_into().unwrap_or_else(|_| {
            log::error!("time Duration is too large {:?}", d);
            1 << 31
        }))
    }
}

impl From<Duration> for std::time::Duration {
    #[inline]
    fn from(d: Duration) -> std::time::Duration {
        std::time::Duration::from_millis(d.0)
    }
}

/// A Seconds type to represent a span of time in seconds.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seconds(pub u16);

impl Seconds {
    pub const ZERO: Seconds = Seconds(0);

    #[inline]
    pub const fn new(secs: u16) -> Seconds {
        Seconds(secs)
    }

    /// Call function `f` if seconds is none zero.
    #[inline]
    pub fn map<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(Duration) -> R,
    {
        if self.0 == 0 {
            None
        } else {
            Some(f(Duration::from(*self)))
        }
    }
}

impl From<Seconds> for std::time::Duration {
    #[inline]
    fn from(d: Seconds) -> std::time::Duration {
        std::time::Duration::from_secs(d.0 as u64)
    }
}
