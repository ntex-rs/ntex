use std::convert::TryInto;

// /// A measurement of a monotonically nondecreasing clock. Opaque and useful only with Duration.
// ///
// /// Instants are always guaranteed to be no less than any previously
// /// measured instant when created, and are often useful for tasks such as measuring
// /// benchmarks or timing how long an operation takes.
// #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
// pub struct Instant(u64);

// impl Instant {
//     pub fn now() -> Instant {
//         todo!()
//     }
// }

/// A Duration type to represent a span of time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(pub(super) u64);

impl Duration {
    /// Zero milliseconds value
    pub const ZERO: Duration = Duration(0);

    /// One second value
    pub const ONE_SEC: Duration = Duration(1_000);

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

    #[inline]
    pub const fn non_zero(self) -> bool {
        self.0 != 0
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

impl Default for Duration {
    #[inline]
    fn default() -> Duration {
        Duration::ZERO
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
    /// Zero seconds value
    pub const ZERO: Seconds = Seconds(0);

    /// One second value
    pub const ONE: Seconds = Seconds(1);

    #[inline]
    pub const fn new(secs: u16) -> Seconds {
        Seconds(secs)
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn non_zero(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub const fn seconds(self) -> u64 {
        self.0 as u64
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

impl Default for Seconds {
    #[inline]
    fn default() -> Seconds {
        Seconds::ZERO
    }
}

impl From<Seconds> for std::time::Duration {
    #[inline]
    fn from(d: Seconds) -> std::time::Duration {
        std::time::Duration::from_secs(d.0 as u64)
    }
}
