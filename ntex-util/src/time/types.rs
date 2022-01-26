use std::{convert::TryInto, ops};

/// A Duration type to represent a span of time.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u32);

impl Millis {
    /// Zero milliseconds value
    pub const ZERO: Millis = Millis(0);

    /// One second value
    pub const ONE_SEC: Millis = Millis(1_000);

    #[inline]
    pub const fn from_secs(secs: u16) -> Millis {
        Millis((secs as u32) * 1000)
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
        F: FnOnce(Millis) -> R,
    {
        if self.0 == 0 {
            None
        } else {
            Some(f(*self))
        }
    }
}

impl Default for Millis {
    #[inline]
    fn default() -> Millis {
        Millis::ZERO
    }
}

impl ops::Add<Millis> for Millis {
    type Output = Millis;

    #[inline]
    fn add(self, other: Millis) -> Millis {
        Millis(self.0.checked_add(other.0).unwrap_or(u32::MAX))
    }
}

impl ops::Add<Seconds> for Millis {
    type Output = Millis;

    #[inline]
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn add(self, other: Seconds) -> Millis {
        Millis(
            self.0
                .checked_add((other.0 as u32).checked_mul(1000).unwrap_or(u32::MAX))
                .unwrap_or(u32::MAX),
        )
    }
}

impl ops::Add<std::time::Duration> for Millis {
    type Output = Millis;

    #[inline]
    fn add(self, other: std::time::Duration) -> Millis {
        self + Millis::from(other)
    }
}

impl ops::Add<Millis> for std::time::Duration {
    type Output = std::time::Duration;

    #[inline]
    fn add(self, other: Millis) -> std::time::Duration {
        self + Self::from(other)
    }
}

impl From<Seconds> for Millis {
    #[inline]
    fn from(s: Seconds) -> Millis {
        Millis((s.0 as u32).checked_mul(1000).unwrap_or(u32::MAX))
    }
}

impl From<std::time::Duration> for Millis {
    #[inline]
    fn from(d: std::time::Duration) -> Millis {
        Self(d.as_millis().try_into().unwrap_or_else(|_| {
            log::error!("time Duration is too large {:?}", d);
            1 << 31
        }))
    }
}

impl From<Millis> for std::time::Duration {
    #[inline]
    fn from(d: Millis) -> std::time::Duration {
        std::time::Duration::from_millis(d.0 as u64)
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
    pub const fn checked_new(secs: usize) -> Seconds {
        let secs = if (u16::MAX as usize) < secs {
            u16::MAX
        } else {
            secs as u16
        };
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
        F: FnOnce(Millis) -> R,
    {
        if self.0 == 0 {
            None
        } else {
            Some(f(Millis::from(*self)))
        }
    }
}

impl Default for Seconds {
    #[inline]
    fn default() -> Seconds {
        Seconds::ZERO
    }
}

impl ops::Add<Seconds> for Seconds {
    type Output = Seconds;

    #[inline]
    fn add(self, other: Seconds) -> Seconds {
        Seconds(self.0.checked_add(other.0).unwrap_or(u16::MAX))
    }
}

impl From<Seconds> for std::time::Duration {
    #[inline]
    fn from(d: Seconds) -> std::time::Duration {
        std::time::Duration::from_secs(d.0 as u64)
    }
}
