//! Timer and rate-tracking state for keep-alive, request-head read-rate, and
//! request-payload read-rate handling.
use crate::io::{IoRef, cfg::FrameReadRate};
use crate::time::Seconds;

/// Dispatcher timer and read-rate state.
///
/// The transport has a single dispatcher timer, `active` records what it is
/// currently armed for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct Timers {
    pub(super) active: Timer,
    pub(super) progress: ReadProgress,
    /// The payload timer is held until `100 Continue` or a response is sent.
    pub(super) payload_hold: bool,
}

/// The purpose of the armed dispatcher timer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum Timer {
    Stopped,
    /// Idle persistent connection, waiting for the next request.
    KeepAlive,
    /// Request-head read rate.
    Headers,
    /// Request-payload read rate.
    Payload,
    /// Payload timing is paused by application or write backpressure, the
    /// transport timer is stopped but the remaining budget is kept.
    PayloadPaused,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct ReadProgress {
    /// Application read buffer length at the last decode attempt.
    pub(super) remains: u32,
    /// Bytes received during the current rate period.
    pub(super) consumed: u32,
    /// Remaining cumulative read budget, after the current period.
    pub(super) max_timeout: Seconds,
}

impl ReadProgress {
    const EMPTY: ReadProgress = ReadProgress {
        remains: 0,
        consumed: 0,
        max_timeout: Seconds::ZERO,
    };
}

/// Splits the first period from the cumulative budget.
///
/// Returns the period to arm and the budget left after it.
pub(super) fn read_timeout(timeout: Seconds, max_timeout: Seconds) -> (Seconds, Seconds) {
    if max_timeout.is_zero() {
        (timeout, Seconds::ZERO)
    } else {
        let timeout = Seconds(timeout.0.min(max_timeout.0));
        (timeout, Seconds(max_timeout.0.saturating_sub(timeout.0)))
    }
}

impl Timers {
    /// Starts request-head timing for the first request when it is
    /// configured, so a new connection must start sending data in time.
    ///
    /// A timer or timeout left on the transport does not apply to the new
    /// dispatcher.
    pub(super) fn new(io: &IoRef, headers: Option<FrameReadRate>) -> Self {
        io.stop_timer();
        let mut timers = Timers {
            active: Timer::Stopped,
            progress: ReadProgress::EMPTY,
            payload_hold: false,
        };
        if let Some(cfg) = headers {
            timers.start(io, Timer::Headers, cfg, cfg.max_timeout);
        }
        timers
    }

    /// Arms the transport timer for a rate period of `timer`.
    fn start(&mut self, io: &IoRef, timer: Timer, cfg: FrameReadRate, max_timeout: Seconds) {
        let (timeout, max_timeout) = read_timeout(cfg.timeout, max_timeout);
        self.active = timer;
        self.progress.max_timeout = max_timeout;
        io.start_timer(timeout);
    }

    /// Stops the transport timer and clears any pending timeout.
    pub(super) fn stop(&mut self, io: &IoRef) {
        self.active = Timer::Stopped;
        io.stop_timer();
    }

    /// Resets the state after a request head has been decoded.
    pub(super) fn reset(&mut self, io: &IoRef) {
        self.progress = ReadProgress::EMPTY;
        self.payload_hold = false;
        self.stop(io);
    }

    /// Starts the keep-alive timer for an idle connection, a running
    /// keep-alive timer keeps running.
    pub(super) fn start_keepalive(&mut self, io: &IoRef, timeout: Seconds) {
        if self.active != Timer::KeepAlive {
            log::debug!("{}: Start keep-alive timer {:?}", io.tag(), timeout);
            self.active = Timer::KeepAlive;
            io.start_timer(timeout);
        }
    }

    /// Starts request-head timing with a fresh budget.
    ///
    /// Without a headers read rate, any timer is stopped.
    pub(super) fn start_headers(
        &mut self,
        io: &IoRef,
        cfg: Option<FrameReadRate>,
        consumed: u32,
        remains: u32,
    ) {
        self.progress.remains = remains;
        self.progress.consumed = consumed;

        if let Some(cfg) = cfg {
            log::debug!("{}: Start headers read timer {:?}", io.tag(), cfg.timeout);
            self.start(io, Timer::Headers, cfg, cfg.max_timeout);
        } else {
            self.stop(io);
        }
    }

    /// Records the application read buffer length before a request-head
    /// decode attempt, counting bytes received since the previous attempt.
    pub(super) fn headers_buffered(&mut self, buffered: u32) {
        if self.active == Timer::Headers {
            let p = &mut self.progress;
            p.consumed = p
                .consumed
                .saturating_add(buffered.saturating_sub(p.remains));
            p.remains = buffered;
        }
    }

    /// Records payload bytes consumed by a decode attempt.
    ///
    /// Starts payload timing if it is not running. Resuming a paused timer
    /// keeps the received bytes and the remaining cumulative budget.
    pub(super) fn payload_decoded(
        &mut self,
        io: &IoRef,
        cfg: Option<FrameReadRate>,
        consumed: u32,
    ) {
        if self.payload_hold {
            return;
        }
        if self.active == Timer::Payload {
            self.progress.consumed = self.progress.consumed.saturating_add(consumed);
        } else if let Some(cfg) = cfg {
            log::debug!("{}: Start payload timer {:?}", io.tag(), cfg.timeout);

            let max_timeout = if self.active == Timer::PayloadPaused {
                self.progress.consumed = self.progress.consumed.saturating_add(consumed);
                if cfg.max_timeout.is_zero() {
                    Seconds::ZERO
                } else {
                    self.progress.max_timeout
                }
            } else {
                self.progress.consumed = consumed;
                cfg.max_timeout
            };
            self.start(io, Timer::Payload, cfg, max_timeout);
        }
    }

    /// Pauses payload timing, the unused part of the current period is
    /// returned to the cumulative budget.
    pub(super) fn pause_payload(&mut self, io: &IoRef, cfg: Option<FrameReadRate>) {
        if self.active == Timer::Payload {
            if let Some(cfg) = cfg
                && cfg.max_timeout.non_zero()
            {
                let remains = io.timer_handle().remains();
                self.progress.max_timeout =
                    Seconds(self.progress.max_timeout.0.saturating_add(remains.0));
            }
            io.stop_timer();
            self.active = Timer::PayloadPaused;
        }
    }

    /// Returns `true` if paused payload timing has no cumulative budget left.
    pub(super) fn payload_budget_exhausted(&self, cfg: Option<FrameReadRate>) -> bool {
        self.active == Timer::PayloadPaused
            && cfg.is_some_and(|cfg| cfg.max_timeout.non_zero())
            && self.progress.max_timeout.is_zero()
    }

    /// Handles expiry of a read-rate period.
    ///
    /// Starts the next period and returns `true` if more than the required
    /// number of bytes was received and the cumulative budget allows it.
    pub(super) fn extend(&mut self, io: &IoRef, cfg: FrameReadRate) -> bool {
        let p = &mut self.progress;
        let total = p.consumed;
        p.consumed = 0;

        if total > cfg.rate {
            let timeout = if cfg.max_timeout.is_zero() {
                Some(cfg.timeout)
            } else if p.max_timeout.is_zero() {
                None
            } else {
                let (timeout, remaining) = read_timeout(cfg.timeout, p.max_timeout);
                p.max_timeout = remaining;
                Some(timeout)
            };

            if let Some(timeout) = timeout {
                log::trace!("{}: Bytes read rate {:?}, extend timer", io.tag(), total);
                io.start_timer(timeout);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_timeout_is_bounded_by_maximum() {
        let (timeout, remaining) = read_timeout(Seconds(10), Seconds(15));
        assert_eq!(timeout, Seconds(10));
        assert_eq!(remaining, Seconds(5));

        let (timeout, remaining) = read_timeout(Seconds(10), remaining);
        assert_eq!(timeout, Seconds(5));
        assert_eq!(remaining, Seconds::ZERO);

        let (timeout, remaining) = read_timeout(Seconds(10), Seconds(3));
        assert_eq!(timeout, Seconds(3));
        assert_eq!(remaining, Seconds::ZERO);

        let (timeout, remaining) = read_timeout(Seconds(10), Seconds::ZERO);
        assert_eq!(timeout, Seconds(10));
        assert_eq!(remaining, Seconds::ZERO);
    }
}
