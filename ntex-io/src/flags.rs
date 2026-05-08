use std::{cell::Cell, fmt};

pub struct Flags(Cell<FlagsKind>);

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct FlagsKind: u16 {
        /// io is closed
        const IO_STOPPED          = 0b0000_0000_0000_0001;
        /// shutdown io tasks
        const IO_STOPPING         = 0b0000_0000_0000_0010;
        /// shutting down filters
        const IO_STOPPING_FILTERS = 0b0000_0000_0000_0100;
        /// need to write buffer
        const IO_WANTS_WRITE      = 0b0000_0000_0000_1000;

        /// pause io read
        const RD_PAUSED           = 0b0000_0000_0001_0000;
        /// read any data and notify dispatcher
        const RD_NOTIFY           = 0b0000_0000_0010_0000;

        /// new data is available in read buffer
        const BUF_R_READY         = 0b0000_0000_0100_0000;
        /// read buffer is full
        const BUF_R_FULL          = 0b0000_0000_1000_0000;

        /// flush write buf
        const WR_FLUSH            = 0b0000_0001_0000_0000;
        /// write task paused
        const WR_PAUSED           = 0b0000_0010_0000_0000;
        /// write any data and notify dispatcher
        const WR_NOTIFY           = 0b0000_0100_0000_0000;

        /// timeout occurred
        const DSP_TIMEOUT         = 0b0000_1000_0000_0000;
        /// write buffer is full
        const DSP_W_BACKPRESSURE  = 0b0001_0000_0000_0000;

        /// early write
        const UPFRONT_WRITE       = 0b0100_0000_0000_0000;
    }
}

impl Clone for Flags {
    fn clone(&self) -> Self {
        Self(Cell::new(self.0.get()))
    }
}

impl Flags {
    pub(crate) fn new(upfront_write: bool) -> Self {
        if upfront_write {
            Self(Cell::new(FlagsKind::WR_PAUSED | FlagsKind::UPFRONT_WRITE))
        } else {
            Self(Cell::new(FlagsKind::WR_PAUSED))
        }
    }

    pub(crate) fn new_stopped() -> Self {
        Self(Cell::new(
            FlagsKind::IO_STOPPED | FlagsKind::IO_STOPPING | FlagsKind::IO_STOPPING_FILTERS,
        ))
    }

    fn get(&self) -> FlagsKind {
        self.0.get()
    }

    fn contains(&self, f: FlagsKind) -> bool {
        self.0.get().contains(f)
    }

    fn intersects(&self, f: FlagsKind) -> bool {
        self.0.get().intersects(f)
    }

    fn insert(&self, f: FlagsKind) {
        let mut flags = self.0.get();
        flags.insert(f);
        self.0.set(flags);
    }

    fn remove(&self, f: FlagsKind) {
        let mut flags = self.0.get();
        flags.remove(f);
        self.0.set(flags);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.intersects(FlagsKind::IO_STOPPING | FlagsKind::IO_STOPPED)
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.contains(FlagsKind::IO_STOPPED)
    }

    pub fn is_stopping(&self) -> bool {
        self.contains(FlagsKind::IO_STOPPING)
    }

    pub(crate) fn is_stopping_any(&self) -> bool {
        self.intersects(
            FlagsKind::IO_STOPPED | FlagsKind::IO_STOPPING | FlagsKind::IO_STOPPING_FILTERS,
        )
    }

    pub(crate) fn is_stopping_filters(&self) -> bool {
        self.contains(FlagsKind::IO_STOPPING_FILTERS)
    }

    pub(crate) fn is_write_notify(&self) -> bool {
        self.contains(FlagsKind::WR_NOTIFY)
    }

    pub(crate) fn is_write_flush(&self) -> bool {
        self.intersects(FlagsKind::WR_FLUSH)
    }

    pub(crate) fn is_wants_write(&self) -> bool {
        self.contains(FlagsKind::IO_WANTS_WRITE)
    }

    pub(crate) fn is_shutting_down_filters(&self) -> bool {
        let f = self.get();
        f.contains(FlagsKind::IO_STOPPING_FILTERS)
            && !f.intersects(FlagsKind::IO_STOPPED | FlagsKind::IO_STOPPING)
    }

    pub(crate) fn is_write_upfront_enabled(&self) -> bool {
        self.contains(FlagsKind::UPFRONT_WRITE | FlagsKind::WR_PAUSED)
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

    pub(crate) fn is_rd_backpressure(&self) -> bool {
        self.contains(FlagsKind::BUF_R_FULL)
    }

    pub(crate) fn is_wr_backpressure(&self) -> bool {
        self.contains(FlagsKind::DSP_W_BACKPRESSURE)
    }

    pub(crate) fn is_read_full_or_paused(&self) -> bool {
        self.intersects(FlagsKind::RD_PAUSED | FlagsKind::BUF_R_FULL)
    }

    pub(crate) fn is_read_full_and_ready(&self) -> bool {
        self.contains(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL)
    }

    pub(crate) fn set_read_paused(&self) {
        self.insert(FlagsKind::RD_PAUSED);
    }

    pub(crate) fn set_write_paused(&self) {
        self.insert(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn set_write_notify(&self) {
        self.insert(FlagsKind::WR_NOTIFY);
    }

    pub(crate) fn set_wr_backpressure(&self) {
        self.insert(FlagsKind::DSP_W_BACKPRESSURE);
    }

    pub(crate) fn set_filter_stopping(&self) {
        self.insert(FlagsKind::IO_STOPPING_FILTERS);
    }

    pub(crate) fn set_force_closed(&self) {
        self.insert(
            FlagsKind::IO_STOPPED
                | FlagsKind::IO_STOPPING
                | FlagsKind::IO_STOPPING_FILTERS
                | FlagsKind::BUF_R_READY,
        );
    }

    pub(crate) fn set_wants_write(&self) {
        self.insert(FlagsKind::IO_WANTS_WRITE);
    }

    pub(crate) fn set_wants_write_flush(&self) {
        self.insert(FlagsKind::WR_FLUSH);
    }

    pub(crate) fn set_read_notify(&self) {
        self.insert(FlagsKind::RD_NOTIFY);
    }

    pub(crate) fn set_read_ready(&self) {
        self.insert(FlagsKind::BUF_R_READY);
    }

    pub(crate) fn set_read_ready_and_full(&self) {
        self.insert(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL);
    }

    pub(crate) fn set_filters_stopped(&self) {
        self.insert(FlagsKind::IO_STOPPING);
    }

    pub(crate) fn unset_write_paused(&self) {
        self.remove(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn unset_write_notify(&self) {
        self.remove(FlagsKind::WR_NOTIFY);
    }

    pub(crate) fn unset_wants_write(&self) {
        self.remove(FlagsKind::IO_WANTS_WRITE);
    }

    pub(crate) fn unset_wr_backpressure(&self) {
        self.remove(FlagsKind::DSP_W_BACKPRESSURE);
    }

    pub(crate) fn unset_wr_backpressure_and_flush(&self) {
        self.remove(FlagsKind::DSP_W_BACKPRESSURE | FlagsKind::WR_FLUSH);
    }

    pub(crate) fn unset_all_read_flags(&self) {
        self.remove(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL);
    }

    pub(crate) fn unset_read_ready(&self) {
        self.remove(FlagsKind::BUF_R_READY);
    }

    pub(crate) fn unset_read_paused(&self) {
        self.remove(FlagsKind::RD_PAUSED);
    }

    /// Checks `RD_NOTIFY` and unsets
    pub(crate) fn check_read_notify(&self) -> bool {
        if self.contains(FlagsKind::RD_NOTIFY) {
            self.remove(FlagsKind::RD_NOTIFY);
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
        self.0.get().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags() {
        assert!(format!("{:?}", Flags::new_stopped()).contains("IO_STOPPED"));
        assert!(format!("{:?}", FlagsKind::IO_STOPPED).contains("IO_STOPPED"));
        assert!(FlagsKind::IO_STOPPED == FlagsKind::IO_STOPPED);
        assert!(FlagsKind::IO_STOPPED != FlagsKind::IO_STOPPING);
    }
}
