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
}

/// The purpose of the armed dispatcher timer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum Timer {
    Stopped,
    /// New connection, waiting for the first byte of the first request.
    /// The transport timer is armed only if request-head timing is
    /// configured, otherwise waiting is unbounded.
    ClientTimeout,
    /// Idle persistent connection, waiting for the next request.
    KeepAlive,
    /// Request-head read rate.
    Headers,
    /// Request-payload read rate.
    Payload,
    /// Payload timing is paused by application or write backpressure, the
    /// transport timer is stopped but the remaining budget is kept.
    PayloadPaused,
    /// Write backpressure timeout.
    Write,
    /// Write backpressure timeout, paused payload timing is restored once
    /// backpressure is disabled.
    WriteWithPayload,
}

impl Timer {
    /// Returns `true` for the write backpressure timer.
    pub(super) fn is_write(self) -> bool {
        matches!(self, Timer::Write | Timer::WriteWithPayload)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct ReadProgress {
    /// Application read buffer length at the last decode attempt.
    pub(super) remains: u32,
    /// Bytes received during the current rate period.
    pub(super) consumed: u32,
    /// Remaining cumulative read budget, after the current period.
    pub(super) max_timeout: Seconds,
    /// Unused part of the current period while payload timing is paused.
    pub(super) period: Seconds,
}

impl ReadProgress {
    const EMPTY: ReadProgress = ReadProgress {
        remains: 0,
        consumed: 0,
        max_timeout: Seconds::ZERO,
        period: Seconds::ZERO,
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
    /// Starts the client timeout when request-head timing is configured, so
    /// a new connection must start sending its first request in time.
    ///
    /// Request-head read-rate timing starts with the first received byte.
    ///
    /// A timer or timeout left on the transport does not apply to the new
    /// dispatcher.
    pub(super) fn new(io: &IoRef, headers: Option<FrameReadRate>) -> Self {
        io.stop_timer();
        if let Some(cfg) = headers {
            io.start_timer(cfg.timeout);
        }
        Timers {
            active: Timer::ClientTimeout,
            progress: ReadProgress::EMPTY,
        }
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
    ///
    /// A running write timer keeps running, buffered requests can be decoded
    /// during write backpressure.
    pub(super) fn reset(&mut self, io: &IoRef) {
        self.progress = ReadProgress::EMPTY;
        self.payload_done(io);
    }

    /// Stops payload timing after the payload has been decoded.
    ///
    /// A running write timer keeps running.
    pub(super) fn payload_done(&mut self, io: &IoRef) {
        if self.active.is_write() {
            self.active = Timer::Write;
        } else {
            self.stop(io);
        }
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

    /// Starts request-payload timing with a fresh budget.
    ///
    /// During write backpressure, payload timing starts paused and resumes
    /// once backpressure is disabled.
    pub(super) fn start_payload(&mut self, io: &IoRef, cfg: Option<FrameReadRate>) {
        if let Some(cfg) = cfg {
            self.progress.consumed = 0;
            if self.active.is_write() {
                let (period, max_timeout) = read_timeout(cfg.timeout, cfg.max_timeout);
                self.progress.period = period;
                self.progress.max_timeout = max_timeout;
                self.active = Timer::WriteWithPayload;
            } else {
                log::debug!("{}: Start payload timer {:?}", io.tag(), cfg.timeout);
                self.start(io, Timer::Payload, cfg, cfg.max_timeout);
            }
        }
    }

    /// Records payload bytes consumed by a decode attempt.
    ///
    /// Resumes paused payload timing, keeping the received bytes, the
    /// unused part of the interrupted period, and the cumulative budget.
    /// Stopped timing is not started.
    pub(super) fn payload_decoded(&mut self, io: &IoRef, consumed: u32) {
        match self.active {
            Timer::Payload => {
                self.progress.consumed = self.progress.consumed.saturating_add(consumed);
            }
            Timer::PayloadPaused => {
                let period = self.progress.period;
                log::trace!("{}: Resume payload timer {:?}", io.tag(), period);
                self.progress.consumed = self.progress.consumed.saturating_add(consumed);
                self.progress.period = Seconds::ZERO;
                self.active = Timer::Payload;
                if period.is_zero() {
                    // the interrupted period has expired, check the read rate
                    io.notify_timeout();
                } else {
                    io.start_timer(period);
                }
            }
            _ => (),
        }
    }

    /// Pauses payload timing, keeping the unused part of the current period.
    ///
    /// The period continues when payload timing resumes, a period that
    /// expired before pausing is checked on resume.
    pub(super) fn pause_payload(&mut self, io: &IoRef) {
        if self.active == Timer::Payload {
            self.progress.period = io.timer_handle().remains();
            io.stop_timer();
            self.active = Timer::PayloadPaused;
        }
    }

    /// Starts the write backpressure timer, a running write timer keeps
    /// running.
    ///
    /// Running payload timing is paused, other read timers are stopped.
    pub(super) fn start_write(&mut self, io: &IoRef, timeout: Seconds) {
        if timeout.non_zero() && !self.active.is_write() {
            log::debug!("{}: Start write timer {:?}", io.tag(), timeout);
            self.pause_payload(io);
            self.active = if self.active == Timer::PayloadPaused {
                Timer::WriteWithPayload
            } else {
                Timer::Write
            };
            io.stop_timer();
            io.start_timer(timeout);
        }
    }

    /// Stops the write backpressure timer and restores paused payload timing.
    pub(super) fn stop_write(&mut self, io: &IoRef) {
        match self.active {
            Timer::Write => {
                io.stop_timer();
                self.active = Timer::Stopped;
            }
            Timer::WriteWithPayload => {
                io.stop_timer();
                self.active = Timer::PayloadPaused;
            }
            _ => (),
        }
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
    use crate::io::{Io, testing::IoTest};

    /// Decoding buffered requests during write backpressure does not restart
    /// the write timer.
    #[crate::rt_test]
    async fn test_write_timer_survives_decoded_requests() {
        let (_client, server) = IoTest::create();
        let io = Io::from(server);
        let io = io.get_ref();
        let rate = Some(FrameReadRate {
            rate: 1,
            timeout: Seconds(2),
            max_timeout: Seconds(10),
        });
        let mut timers = Timers::new(&io, None);

        timers.start_write(&io, Seconds(5));
        assert_eq!(timers.active, Timer::Write);
        let deadline = io.timer_handle();
        assert!(deadline.is_set());

        timers.reset(&io);
        assert_eq!(timers.active, Timer::Write);

        timers.start_payload(&io, rate);
        assert_eq!(timers.active, Timer::WriteWithPayload);
        assert_eq!(timers.progress.period, Seconds(2));
        assert_eq!(timers.progress.max_timeout, Seconds(8));

        timers.payload_done(&io);
        assert_eq!(timers.active, Timer::Write);
        timers.start_write(&io, Seconds(5));
        assert_eq!(io.timer_handle(), deadline);

        timers.start_payload(&io, rate);
        timers.stop_write(&io);
        assert_eq!(timers.active, Timer::PayloadPaused);
        assert!(!io.timer_handle().is_set());
    }

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
