//! Timer and rate-tracking state for keep-alive, frame read-rate, and write
//! timeout handling.
use ntex_io::{IoBoxed, cfg::IoConfig};
use ntex_util::time::Seconds;

/// Dispatcher timer and frame read state.
///
/// The transport has a single dispatcher timer, `active` records what it is
/// currently armed for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Timers {
    pub(crate) active: Timer,
    pub(crate) read: ReadPhase,
}

/// Progress of frame decoding on the connection.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReadPhase {
    /// No partial frame is buffered.
    Idle,
    /// A frame has started but is not complete. A new connection starts in
    /// this phase when a frame read rate is configured, so its first frame is
    /// tracked before any data arrives.
    ReadingFrame(ReadProgress),
}

/// The purpose of the armed dispatcher timer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Timer {
    Stopped,
    KeepAlive,
    FrameRead,
    /// Write timeout, from enabling write backpressure until it is disabled.
    Write,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReadProgress {
    /// Application read buffer length at the last decode attempt.
    pub(crate) remains: u32,
    /// Bytes received during the current rate period, including bytes the
    /// codec consumed without producing a frame.
    pub(crate) consumed: u32,
    /// Remaining cumulative frame-read budget.
    pub(crate) max_timeout: Seconds,
}

impl Timers {
    /// Starts frame read-rate tracking for the first frame when it is
    /// configured, so a new connection must start sending data in time.
    pub(crate) fn new(io: &IoBoxed) -> Self {
        if let Some(params) = io.cfg().frame_read_rate() {
            io.start_timer(params.timeout);
            Timers {
                active: Timer::FrameRead,
                read: ReadPhase::ReadingFrame(ReadProgress {
                    max_timeout: params.max_timeout,
                    ..ReadProgress::EMPTY
                }),
            }
        } else {
            io.stop_timer();
            Timers {
                active: Timer::Stopped,
                read: ReadPhase::Idle,
            }
        }
    }

    /// Updates the read phase after a decode attempt.
    ///
    /// `remains` is the buffered input left by the decoder and `consumed` the
    /// input it took without producing a frame.
    pub(crate) fn update_read(&mut self, cfg: &IoConfig, item: bool, remains: u32, consumed: u32) {
        if item {
            self.read = ReadPhase::Idle;
            return;
        }
        let partial = remains != 0 || consumed != 0;

        let Some(params) = cfg.frame_read_rate() else {
            self.read = if partial {
                ReadPhase::ReadingFrame(ReadProgress::EMPTY)
            } else {
                ReadPhase::Idle
            };
            return;
        };

        if self.read == ReadPhase::Idle {
            if !partial {
                return;
            }
            self.read = ReadPhase::ReadingFrame(ReadProgress {
                max_timeout: params.max_timeout,
                ..ReadProgress::EMPTY
            });
        }
        if let Some(p) = self.read.progress() {
            let received = remains.saturating_add(consumed).saturating_sub(p.remains);
            p.consumed = p.consumed.saturating_add(received);
            p.remains = remains;
        }
    }

    /// Restarts rate tracking of a partial frame with a fresh period and
    /// `max_timeout` budget, used when the service is not ready.
    pub(crate) fn reset_read(&mut self, cfg: &IoConfig) {
        if let (Some(params), Some(p)) = (cfg.frame_read_rate(), self.read.progress()) {
            p.consumed = 0;
            p.max_timeout = params.max_timeout;
        }
    }

    /// Selects the read-side timer.
    ///
    /// A frame being read is bounded by the frame read rate, when one is
    /// configured. Keep-alive applies only while the connection is idle: no
    /// partial frame is buffered and no frame is handled.
    pub(crate) fn select(&self, cfg: &IoConfig, keepalive: bool, handling: bool) -> Timer {
        match self.read {
            ReadPhase::ReadingFrame(_) => {
                if cfg.frame_read_rate().is_some() {
                    Timer::FrameRead
                } else {
                    Timer::Stopped
                }
            }
            ReadPhase::Idle => {
                if keepalive && !handling {
                    Timer::KeepAlive
                } else {
                    Timer::Stopped
                }
            }
        }
    }
}

impl ReadPhase {
    pub(crate) fn progress(&mut self) -> Option<&mut ReadProgress> {
        match self {
            ReadPhase::Idle => None,
            ReadPhase::ReadingFrame(p) => Some(p),
        }
    }
}

impl ReadProgress {
    pub(crate) const EMPTY: ReadProgress = ReadProgress {
        remains: 0,
        consumed: 0,
        max_timeout: Seconds::ZERO,
    };
}
