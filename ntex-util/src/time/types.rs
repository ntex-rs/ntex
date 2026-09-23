use std::ops;

/// A span of time in milliseconds, for timeouts.
///
/// The value is a `u32`, so the longest representable span is about 49 days.
/// Arithmetic saturates instead of overflowing.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u32);

impl Millis {
    /// Zero milliseconds value
    pub const ZERO: Millis = Millis(0);

    /// One second value
    pub const ONE_SEC: Millis = Millis(1_000);

    #[inline]
    /// Converts seconds to milliseconds, saturating at [`u32::MAX`].
    pub const fn from_secs(secs: u32) -> Millis {
        Millis(secs.saturating_mul(1000))
    }

    #[inline]
    /// Returns `true` if the duration is zero.
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    #[inline]
    /// Returns `true` if the duration is not zero.
    pub const fn non_zero(self) -> bool {
        self.0 != 0
    }

    /// Calls `f` if the duration is not zero.
    #[inline]
    pub fn map<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(Millis) -> R,
    {
        if self.0 == 0 { None } else { Some(f(*self)) }
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
        Millis(self.0.saturating_add(other.0))
    }
}

impl ops::Add<Seconds> for Millis {
    type Output = Millis;

    #[inline]
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn add(self, other: Seconds) -> Millis {
        Millis(
            self.0
                .saturating_add((u32::from(other.0)).saturating_mul(1000)),
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

impl From<u32> for Millis {
    #[inline]
    fn from(s: u32) -> Millis {
        Millis(s)
    }
}

impl From<i32> for Millis {
    #[inline]
    fn from(i: i32) -> Millis {
        Millis(if i < 0 { 0 } else { i as u32 })
    }
}

impl From<usize> for Millis {
    #[inline]
    fn from(s: usize) -> Millis {
        Self(s.try_into().unwrap_or_else(|_| {
            log::error!("Values is too large {s:?}");
            u32::MAX
        }))
    }
}

impl From<Seconds> for Millis {
    #[inline]
    fn from(s: Seconds) -> Millis {
        Millis((u32::from(s.0)).saturating_mul(1000))
    }
}

impl From<std::time::Duration> for Millis {
    #[inline]
    fn from(d: std::time::Duration) -> Millis {
        Self(d.as_millis().try_into().unwrap_or_else(|_| {
            log::error!("Duration is too large for Millis: {d:?}");
            u32::MAX
        }))
    }
}

impl From<Millis> for std::time::Duration {
    #[inline]
    fn from(d: Millis) -> std::time::Duration {
        std::time::Duration::from_millis(u64::from(d.0))
    }
}

/// A span of time in whole seconds.
///
/// The value is a `u16`, so the longest representable span is about 18 hours.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seconds(pub u16);

impl Seconds {
    /// Zero seconds value
    pub const ZERO: Seconds = Seconds(0);

    /// One second value
    pub const ONE: Seconds = Seconds(1);

    #[inline]
    /// Creates a duration of `secs` seconds.
    pub const fn new(secs: u16) -> Seconds {
        Seconds(secs)
    }

    #[inline]
    /// Creates a duration of `secs` seconds, saturating at [`u16::MAX`].
    pub const fn checked_new(secs: usize) -> Seconds {
        let secs = if (u16::MAX as usize) < secs {
            u16::MAX
        } else {
            secs as u16
        };
        Seconds(secs)
    }

    #[inline]
    /// Returns `true` if the duration is zero.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    #[inline]
    /// Returns `true` if the duration is not zero.
    pub const fn non_zero(self) -> bool {
        self.0 != 0
    }

    #[inline]
    /// Returns the number of seconds.
    pub const fn seconds(self) -> u64 {
        self.0 as u64
    }

    /// Calls `f` with the duration in milliseconds if it is not zero.
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
        Seconds(self.0.saturating_add(other.0))
    }
}

impl From<Seconds> for std::time::Duration {
    #[inline]
    fn from(d: Seconds) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(d.0))
    }
}

impl From<u16> for Seconds {
    #[inline]
    fn from(secs: u16) -> Seconds {
        Self(secs)
    }
}

impl From<i32> for Seconds {
    #[inline]
    fn from(i: i32) -> Seconds {
        Seconds(if i < 0 {
            0
        } else {
            (i as u32).min(u32::from(u16::MAX)) as u16
        })
    }
}

impl From<usize> for Seconds {
    #[inline]
    fn from(secs: usize) -> Seconds {
        Self(secs.try_into().unwrap_or_else(|_| {
            log::error!("Seconds value is too large {secs:?}");
            u16::MAX
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn time_types() {
        let m = Millis::default();
        assert_eq!(m.0, 0);

        let m = Millis::from(10u32);
        assert_eq!(m.0, 10);

        let m = Millis::from(10usize);
        assert_eq!(m.0, 10);

        let m = Millis(10) + Millis(20);
        assert_eq!(m.0, 30);

        let m = Millis(10) + Millis(u32::MAX);
        assert_eq!(m.0, u32::MAX);

        let m = Millis(10) + Seconds(1);
        assert_eq!(m.0, 1010);

        let m = Millis(u32::MAX) + Seconds(1);
        assert_eq!(m.0, u32::MAX);

        let m = Millis(10) + Duration::from_millis(100);
        assert_eq!(m.0, 110);

        let m = Duration::from_millis(100) + Millis(10);
        assert_eq!(m, Duration::from_millis(110));

        let m = Millis::from(Seconds(1));
        assert_eq!(m.0, 1000);

        let m = Millis::from(Duration::from_secs(1));
        assert_eq!(m.0, 1000);

        let m = Millis::from(Duration::from_secs(u64::MAX));
        assert_eq!(m.0, u32::MAX);

        let m = Millis::from_secs(1);
        assert_eq!(m.0, 1000);
        assert_eq!(Millis::from_secs(u32::MAX).0, u32::MAX);

        let m = Millis(0);
        assert_eq!(m.map(|m| m + Millis(1)), None);

        let m = Millis(1);
        assert_eq!(m.map(|m| m + Millis(1)), Some(Millis(2)));

        let s = Seconds::new(10);
        assert_eq!(s.0, 10);

        let s = Seconds::from(10u16);
        assert_eq!(s.0, 10);

        let s = Seconds::from(10usize);
        assert_eq!(s.0, 10);

        let s = Seconds::from(10i32);
        assert_eq!(s.0, 10);

        let s = Seconds::from(-10i32);
        assert_eq!(s.0, 0);
        assert_eq!(Seconds::from(i32::MAX).0, u16::MAX);

        let s = Seconds::checked_new(10);
        assert_eq!(s.0, 10);

        let s = Seconds::checked_new(u16::MAX as usize + 10);
        assert_eq!(s.0, u16::MAX);

        assert!(Seconds::ZERO.is_zero());
        assert!(!Seconds::ZERO.non_zero());

        let s = Seconds::new(10);
        assert_eq!(s.seconds(), 10);

        let s = Seconds::default();
        assert_eq!(s.0, 0);

        let s = Seconds::new(10) + Seconds::new(10);
        assert_eq!(s.seconds(), 20);

        assert_eq!(Seconds(0).map(|_| 1usize), None);
        assert_eq!(Seconds(2).map(|_| 1usize), Some(1));

        let d = Duration::from(Seconds(100));
        assert_eq!(d.as_secs(), 100);
    }
}
