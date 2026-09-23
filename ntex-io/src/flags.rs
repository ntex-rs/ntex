use std::{cell::Cell, fmt};

/// What the connection is currently doing.
///
/// A connection only ever moves forward through these states.
///
/// Later states are not necessarily passed *through*: the transport can report
/// the connection as gone at any point, taking it straight from
/// [`Phase::Active`] to [`Phase::Stopped`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Phase {
    /// Io is in active state.
    Active,
    /// The filter chain is shutting down.
    FiltersStopping(Manner),
    /// The filters are done and the transport is draining buffered output.
    TransportShutdown(Manner),
    /// The backend released the socket, transport teardown is complete.
    ///
    /// Keeps how the connection ended, so it can still be told apart once
    /// the connection is gone. A connection the transport reported as gone
    /// without a close having been started ended `Graceful`.
    Stopped(Manner),
}

impl Phase {
    /// Position in the progression, ignoring how the connection ends.
    fn stage(self) -> u8 {
        match self {
            Phase::Active => 0,
            Phase::FiltersStopping(_) => 1,
            Phase::TransportShutdown(_) => 2,
            Phase::Stopped(_) => 3,
        }
    }

    /// Checks whether this is at least as far along as `other`.
    fn reached(self, other: Phase) -> bool {
        self.stage() >= other.stage()
    }

    /// How the connection is ending or ended, `Graceful` while it is active.
    fn manner(self) -> Manner {
        match self {
            Phase::FiltersStopping(m) | Phase::TransportShutdown(m) | Phase::Stopped(m) => m,
            Phase::Active => Manner::Graceful,
        }
    }
}

/// How the connection is being ended.
///
///  Ordered by severity, so raising it can never downgrade an abort that is already in progress.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum Manner {
    /// Ending normally, buffered output is still delivered.
    Graceful,
    /// A failure or an expired deadline is tearing the connection down.
    Terminating,
    /// The application asked for a force close through `IoRef::terminate()`.
    ForceClosed,
}

pub struct Flags {
    bits: Cell<FlagsKind>,
    phase: Cell<Phase>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct FlagsKind: u16 {
        /// pause io read
        const RD_PAUSED           = 1 << 1;
        /// read backpressure
        const RD_BACKPRESSURE     = 1 << 2;
        /// transport read side reached clean EOF
        const RD_EOF              = 1 << 3;

        /// a `read_notify()` waiter wants to hear about the next transport read
        const RD_NOTIFY           = 1 << 4;
        /// a transport read completed while `RD_NOTIFY` was set
        const RD_NOTIFIED         = 1 << 5;

        /// new data is available in read buffer
        const BUF_R_READY         = 1 << 6;

        /// flush write buf
        const WR_FLUSH            = 1 << 7;
        /// write task paused
        const WR_PAUSED           = 1 << 8;
        /// write op is scheduled
        const WR_SEND_OP          = 1 << 9;

        /// timeout occurred
        const DSP_TIMEOUT         = 1 << 10;
        /// write buffer is full
        const DSP_W_BACKPRESSURE  = 1 << 11;
        /// is direct-write enabled
        const DIRECT_WR_SUP       = 1 << 12;
    }
}

impl Clone for Flags {
    fn clone(&self) -> Self {
        Self {
            bits: Cell::new(self.bits.get()),
            phase: Cell::new(self.phase.get()),
        }
    }
}

impl Flags {
    pub(crate) fn new(direct_wr: bool) -> Self {
        let bits = if direct_wr {
            FlagsKind::WR_PAUSED | FlagsKind::DIRECT_WR_SUP
        } else {
            FlagsKind::WR_PAUSED
        };
        Self {
            bits: Cell::new(bits),
            phase: Cell::new(Phase::Active),
        }
    }

    pub(crate) fn new_stopped() -> Self {
        Self {
            bits: Cell::new(FlagsKind::empty()),
            phase: Cell::new(Phase::Stopped(Manner::Graceful)),
        }
    }

    fn contains(&self, f: FlagsKind) -> bool {
        self.bits.get().contains(f)
    }

    fn intersects(&self, f: FlagsKind) -> bool {
        self.bits.get().intersects(f)
    }

    fn insert(&self, f: FlagsKind) {
        let mut flags = self.bits.get();
        flags.insert(f);
        self.bits.set(flags);
    }

    fn remove(&self, f: FlagsKind) {
        let mut flags = self.bits.get();
        flags.remove(f);
        self.bits.set(flags);
    }

    fn reached(&self, phase: Phase) -> bool {
        self.phase.get().reached(phase)
    }

    fn ending_at_least(&self, manner: Manner) -> bool {
        self.phase.get().manner() >= manner
    }

    /// Checks whether the connection stopped exchanging data with the peer.
    ///
    /// Unlike [`is_active`](Self::is_active) this excludes the filter
    /// shutdown phase, during which the filter chain may still produce output
    /// that has to reach the peer.
    pub(crate) fn is_peer_gone(&self) -> bool {
        self.is_closed()
            || self.reached(Phase::TransportShutdown(Manner::Graceful))
            || self.is_terminating()
    }

    /// Checks whether the backend released the socket.
    pub(crate) fn is_closed(&self) -> bool {
        matches!(self.phase.get(), Phase::Stopped(_))
    }

    /// Checks whether the connection is aborting or already gone.
    ///
    /// Unlike [`is_peer_gone`](Self::is_peer_gone) this does not cover the graceful
    /// transport shutdown phase, during which buffered output is still written
    /// out.
    pub(crate) fn is_aborted(&self) -> bool {
        self.is_closed() || self.is_terminating()
    }

    /// Checks whether the connection reached transport shutdown.
    ///
    /// This stays set once backend teardown completes, including for a
    /// connection the transport reported as gone without a shutdown having
    /// been started.
    pub(crate) fn is_stopping(&self) -> bool {
        self.reached(Phase::TransportShutdown(Manner::Graceful))
    }

    /// Checks whether the connection is ending, or ended, through the
    /// force-termination path.
    ///
    /// This stays set once backend teardown completes.
    pub(crate) fn is_terminating(&self) -> bool {
        self.ending_at_least(Manner::Terminating)
    }

    /// Checks whether the application asked for a force close.
    ///
    /// Only [`IoRef::terminate`](crate::IoRef::terminate) sets this. A
    /// transport failure, a filter failure or an expired shutdown deadline
    /// terminate the connection as well, but they close it gracefully rather
    /// than aborting it.
    ///
    /// This stays set once backend teardown completes.
    pub(crate) fn is_force_closing(&self) -> bool {
        self.ending_at_least(Manner::ForceClosed)
    }

    pub(crate) fn is_stopping_or_terminating(&self) -> bool {
        self.is_stopping() || self.is_terminating()
    }

    /// Checks whether the connection is still in its active state.
    ///
    /// False from the moment any kind of close starts, and for a connection
    /// the transport reported as gone without a shutdown having been started.
    pub(crate) fn is_active(&self) -> bool {
        self.phase.get() == Phase::Active
    }

    pub(crate) fn is_stopping_filters(&self) -> bool {
        self.reached(Phase::FiltersStopping(Manner::Graceful))
    }

    pub(crate) fn is_write_flush(&self) -> bool {
        self.intersects(FlagsKind::WR_FLUSH)
    }

    /// Checks whether the filter chain is the stage currently shutting down.
    ///
    /// Unlike [`is_stopping_filters`](Self::is_stopping_filters) this is only
    /// true while that stage is the current one, so it goes false once the
    /// transport shutdown starts or the connection ends.
    pub(crate) fn is_shutting_down_filters(&self) -> bool {
        self.phase.get() == Phase::FiltersStopping(Manner::Graceful)
    }

    pub(crate) fn is_direct_wr_enabled(&self) -> bool {
        self.contains(FlagsKind::DIRECT_WR_SUP)
    }

    pub(crate) fn set_direct_wr_enabled(&self, enabled: bool) {
        if enabled {
            self.insert(FlagsKind::DIRECT_WR_SUP);
        } else {
            self.remove(FlagsKind::DIRECT_WR_SUP);
        }
    }

    pub(crate) fn is_read_paused(&self) -> bool {
        self.contains(FlagsKind::RD_PAUSED)
    }

    pub(crate) fn is_write_paused(&self) -> bool {
        self.contains(FlagsKind::WR_PAUSED)
    }

    pub(crate) fn is_read_ready(&self) -> bool {
        self.contains(FlagsKind::BUF_R_READY)
    }

    pub(crate) fn is_read_notify(&self) -> bool {
        self.contains(FlagsKind::RD_NOTIFY)
    }

    #[cfg(test)]
    pub(crate) fn is_read_notified(&self) -> bool {
        self.contains(FlagsKind::RD_NOTIFIED)
    }

    pub(crate) fn is_rd_backpressure(&self) -> bool {
        self.contains(FlagsKind::RD_BACKPRESSURE)
    }

    pub(crate) fn is_read_eof(&self) -> bool {
        self.contains(FlagsKind::RD_EOF)
    }

    pub(crate) fn is_wr_backpressure(&self) -> bool {
        self.contains(FlagsKind::DSP_W_BACKPRESSURE)
    }

    pub(crate) fn is_wr_send_scheduled(&self) -> bool {
        self.contains(FlagsKind::WR_SEND_OP)
    }

    pub(crate) fn is_read_paused_or_backpressure(&self) -> bool {
        self.intersects(FlagsKind::RD_PAUSED | FlagsKind::RD_BACKPRESSURE)
    }

    pub(crate) fn set_read_paused(&self) {
        self.insert(FlagsKind::RD_PAUSED);
    }

    pub(crate) fn set_write_paused(&self) {
        self.insert(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn set_wr_send_scheduled(&self) {
        self.insert(FlagsKind::WR_SEND_OP);
    }

    pub(crate) fn set_wr_backpressure(&self) {
        self.insert(FlagsKind::DSP_W_BACKPRESSURE);
    }

    /// Starts a graceful shutdown with the filter chain.
    pub(crate) fn enter_filters_stopping(&self) {
        if self.phase.get() == Phase::Active {
            self.phase.set(Phase::FiltersStopping(Manner::Graceful));
        }
    }

    /// Moves the connection onto the termination path.
    ///
    /// `force` marks an abort requested through
    /// [`IoRef::terminate`](crate::IoRef::terminate), which the transport turns
    /// into a reset rather than a graceful close.
    ///
    /// Returns whether this call started the termination, so that the caller
    /// runs the one-off teardown work exactly once. An already terminating
    /// connection still has its manner escalated, so a force close is honoured
    /// even when a graceful termination got there first; a connection the
    /// transport already reported as gone is left alone.
    pub(crate) fn begin_terminate(&self, force: bool) -> bool {
        let manner = if force {
            Manner::ForceClosed
        } else {
            Manner::Terminating
        };
        let phase = self.phase.get();
        let started = phase.manner() < Manner::Terminating;
        self.phase.set(match phase {
            Phase::Stopped(_) => return false,
            Phase::Active => Phase::FiltersStopping(manner),
            Phase::FiltersStopping(m) => Phase::FiltersStopping(m.max(manner)),
            Phase::TransportShutdown(m) => Phase::TransportShutdown(m.max(manner)),
        });

        if started {
            self.insert(FlagsKind::BUF_R_READY);
        }
        started
    }

    pub(crate) fn set_stopped(&self) {
        let manner = self.phase.get().manner();
        self.phase.set(Phase::Stopped(manner));
    }

    pub(crate) fn set_wants_write_flush(&self) {
        self.insert(FlagsKind::WR_FLUSH);
    }

    pub(crate) fn set_read_notify(&self) {
        self.insert(FlagsKind::RD_NOTIFY);
    }

    pub(crate) fn set_read_notified(&self) {
        self.insert(FlagsKind::RD_NOTIFIED);
    }

    pub(crate) fn set_read_ready(&self) {
        self.insert(FlagsKind::BUF_R_READY);
    }

    pub(crate) fn set_read_ready_and_backpressure(&self) {
        self.insert(FlagsKind::RD_PAUSED | FlagsKind::BUF_R_READY | FlagsKind::RD_BACKPRESSURE);
    }

    pub(crate) fn set_read_eof(&self) {
        self.insert(FlagsKind::RD_EOF);
    }

    /// Moves on to transport shutdown, keeping how the connection ends.
    pub(crate) fn enter_transport_shutdown(&self) {
        match self.phase.get() {
            Phase::Active => self.phase.set(Phase::TransportShutdown(Manner::Graceful)),
            Phase::FiltersStopping(m) => self.phase.set(Phase::TransportShutdown(m)),
            Phase::TransportShutdown(_) | Phase::Stopped(_) => {}
        }
    }

    pub(crate) fn unset_write_paused(&self) {
        self.remove(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn unset_wr_send_scheduled(&self) {
        self.remove(FlagsKind::WR_SEND_OP);
    }

    pub(crate) fn unset_wr_backpressure(&self) {
        self.remove(FlagsKind::DSP_W_BACKPRESSURE);
    }

    pub(crate) fn unset_wr_backpressure_and_flush(&self) {
        self.remove(FlagsKind::DSP_W_BACKPRESSURE | FlagsKind::WR_FLUSH);
    }

    /// `RD_PAUSED` is deliberately left set, even though
    /// `set_read_ready_and_backpressure()` installs it. Callers rely on the
    /// pause surviving so that they can detect it and wake the read task.
    pub(crate) fn unset_read_ready_and_backpressure(&self) {
        self.remove(FlagsKind::BUF_R_READY | FlagsKind::RD_BACKPRESSURE);
    }

    pub(crate) fn unset_read_ready(&self) {
        self.remove(FlagsKind::BUF_R_READY);
    }

    pub(crate) fn unset_read_paused(&self) {
        self.remove(FlagsKind::RD_PAUSED);
    }

    /// Reports whether a transport read completed for the `read_notify()`
    /// waiter, and ends that wait by clearing both `RD_NOTIFY` and
    /// `RD_NOTIFIED`.
    pub(crate) fn take_read_notified(&self) -> bool {
        if self.contains(FlagsKind::RD_NOTIFIED) {
            self.remove(FlagsKind::RD_NOTIFY | FlagsKind::RD_NOTIFIED);
            true
        } else {
            false
        }
    }

    /// Checks `DSP_TIMEOUT` and unsets
    pub(crate) fn check_dispatcher_timeout(&self) -> bool {
        if self.contains(FlagsKind::DSP_TIMEOUT) {
            self.remove(FlagsKind::DSP_TIMEOUT);
            true
        } else {
            false
        }
    }

    /// Checks `DSP_TIMEOUT` and sets
    pub(crate) fn check_dispatcher_timeout_unset(&self) -> bool {
        if self.contains(FlagsKind::DSP_TIMEOUT) {
            false
        } else {
            self.insert(FlagsKind::DSP_TIMEOUT);
            true
        }
    }
}

impl fmt::Debug for Flags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} | {:?}", self.bits.get(), self.phase.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        assert!(format!("{:?}", Flags::new_stopped()).contains("Stopped"));
        assert!(format!("{:?}", Flags::new(false)).contains("Active"));
        assert!(format!("{:?}", FlagsKind::RD_EOF).contains("RD_EOF"));
        assert_eq!(FlagsKind::RD_EOF, FlagsKind::RD_EOF);
        assert_ne!(FlagsKind::RD_EOF, FlagsKind::RD_PAUSED);
    }

    #[test]
    fn flag_bits_are_distinct() {
        let all = FlagsKind::all().bits();
        assert_eq!(
            all.count_ones() as usize,
            FlagsKind::all().iter().count(),
            "two flags share a bit"
        );
    }

    #[test]
    fn shutdown_phase_only_moves_forward() {
        let f = Flags::new(false);
        assert!(!f.is_stopping_filters());
        assert!(!f.is_stopping());

        f.enter_filters_stopping();
        assert!(f.is_stopping_filters());
        assert!(f.is_shutting_down_filters());
        assert!(!f.is_stopping());

        f.enter_transport_shutdown();
        assert!(f.is_stopping());
        // the earlier stage stays reported, and is no longer the current one
        assert!(f.is_stopping_filters());
        assert!(!f.is_shutting_down_filters());

        // an earlier stage cannot pull the connection back
        f.enter_filters_stopping();
        assert!(f.is_stopping());

        // stopped is terminal
        f.set_stopped();
        f.enter_filters_stopping();
        f.enter_transport_shutdown();
        assert!(f.is_closed());
    }

    /// Being stopped is the last state, not a flag beside the others.
    ///
    /// The transport can report the connection gone at any point, so a
    /// stopped connection has not necessarily *passed through* the earlier
    /// states. It still reports them as reached, because every predicate here
    /// asks "has it got at least this far" and once the socket is gone every
    /// earlier state is over.
    #[test]
    fn stopped_is_the_last_state_whichever_route_reached_it() {
        // walked the whole shutdown
        let graceful = Flags::new(false);
        graceful.enter_filters_stopping();
        graceful.enter_transport_shutdown();
        graceful.set_stopped();

        // the transport reported the connection gone while it was still active
        let hup = Flags::new(false);
        hup.set_stopped();

        for f in [&graceful, &hup] {
            assert!(f.is_closed());
            assert!(!f.is_active());
            assert!(f.is_stopping());
            assert!(f.is_stopping_filters());
            // nothing is in progress anymore
            assert!(!f.is_shutting_down_filters());
        }
    }

    #[test]
    fn a_stopped_connection_is_gone_without_having_been_aborted() {
        let f = Flags::new(false);
        f.set_stopped();
        assert!(f.is_closed());
        assert!(f.is_peer_gone());
        assert!(f.is_aborted());
        assert!(!f.is_active());
        // the transport reported it gone, nothing asked for an abort
        assert!(!f.is_terminating());
        assert!(!f.is_force_closing());
    }

    #[test]
    fn terminate_runs_teardown_once_and_force_still_escalates() {
        let f = Flags::new(false);
        assert!(f.begin_terminate(false));
        assert!(f.is_terminating());
        assert!(!f.is_force_closing());
        // reaching the filter stage is implied by terminating
        assert!(f.is_stopping_filters());
        assert!(f.is_read_ready());

        // second call does not repeat the teardown work
        assert!(!f.begin_terminate(false));

        // but a force close still escalates how the connection ends
        assert!(!f.begin_terminate(true));
        assert!(f.is_force_closing());
        assert!(f.is_terminating());
    }

    #[test]
    fn force_close_is_never_downgraded() {
        let f = Flags::new(false);
        assert!(f.begin_terminate(true));
        assert!(f.is_force_closing());
        f.begin_terminate(false);
        assert!(
            f.is_force_closing(),
            "graceful terminate downgraded an abort"
        );
    }

    /// Every state the type can represent is reachable, and nothing else is.
    ///
    /// With the manner carried by the shutdown states, an illegal combination
    /// such as an active connection that is being aborted cannot be written
    /// down at all. This checks the other direction: that no representable
    /// state is dead, which would mean the type is still too loose.
    #[test]
    fn every_representable_state_is_reachable() {
        const OPS: usize = 5;
        let mut seen = std::collections::HashSet::new();

        for len in 1..=5u32 {
            for mut code in 0..OPS.pow(len) {
                let f = Flags::new(false);
                for _ in 0..len {
                    let op = code % OPS;
                    code /= OPS;
                    match op {
                        0 => f.enter_filters_stopping(),
                        // only reachable from inside filter shutdown
                        1 => {
                            if f.is_stopping_filters() {
                                f.enter_transport_shutdown();
                            }
                        }
                        2 => {
                            f.begin_terminate(false);
                        }
                        3 => {
                            f.begin_terminate(true);
                        }
                        _ => f.set_stopped(),
                    }
                    seen.insert(f.phase.get());
                }
            }
        }

        let manners = [Manner::Graceful, Manner::Terminating, Manner::ForceClosed];
        let expected: std::collections::HashSet<_> = [Phase::Active]
            .into_iter()
            .chain(manners.map(Phase::FiltersStopping))
            .chain(manners.map(Phase::TransportShutdown))
            .chain(manners.map(Phase::Stopped))
            .collect();
        assert_eq!(seen.len(), 10);
        assert_eq!(seen, expected);
    }

    /// Finishing the filters moves to the next state, which carries its own
    /// manner, so the transition has to hand the manner over. Losing it here
    /// would silently turn a force close into a clean one.
    #[test]
    fn finishing_the_filters_keeps_how_the_connection_ends() {
        for (force, manner) in [(false, Manner::Terminating), (true, Manner::ForceClosed)] {
            let f = Flags::new(false);
            f.begin_terminate(force);
            assert_eq!(f.phase.get(), Phase::FiltersStopping(manner));
            f.enter_transport_shutdown();
            assert_eq!(f.phase.get(), Phase::TransportShutdown(manner));
            assert_eq!(f.is_force_closing(), force);
        }
    }

    /// Teardown completing is the last transition, so it has to hand the
    /// manner over as well, from whichever state the connection was in.
    #[test]
    fn a_stopped_connection_remembers_how_it_ended() {
        for (force, manner) in [(false, Manner::Terminating), (true, Manner::ForceClosed)] {
            // stopped during filter shutdown
            let f = Flags::new(false);
            f.begin_terminate(force);
            f.set_stopped();
            assert_eq!(f.phase.get(), Phase::Stopped(manner));

            // stopped during transport shutdown
            let f = Flags::new(false);
            f.begin_terminate(force);
            f.enter_transport_shutdown();
            f.set_stopped();
            assert_eq!(f.phase.get(), Phase::Stopped(manner));
            assert!(f.is_closed() && f.is_terminating());
            assert_eq!(f.is_force_closing(), force);
        }

        // a clean close stays clean
        let f = Flags::new(false);
        f.enter_filters_stopping();
        f.enter_transport_shutdown();
        f.set_stopped();
        assert_eq!(f.phase.get(), Phase::Stopped(Manner::Graceful));
        assert!(!f.is_terminating());
    }

    #[test]
    fn a_terminated_connection_is_left_alone() {
        let f = Flags::new(false);
        f.set_stopped();
        assert!(!f.begin_terminate(true));
        assert!(!f.is_force_closing());
        assert!(!f.is_terminating());
    }
}
