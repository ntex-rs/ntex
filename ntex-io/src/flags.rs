use std::cell::Cell;

#[derive(Debug)]
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

        /// wait while write task flushes buf
        const BUF_W_MUST_FLUSH    = 0b0000_0001_0000_0000;
        /// write buffer is full
        const BUF_W_BACKPRESSURE  = 0b0000_0010_0000_0000;

        /// write task paused
        const WR_PAUSED           = 0b0000_0100_0000_0000;
        /// wait for write completion task
        const WR_TASK_WAIT        = 0b0000_1000_0000_0000;

        /// timeout occurred
        const DSP_TIMEOUT         = 0b0010_0000_0000_0000;

        /// handle is set
        const HANDLE              = 0b0100_0000_0000_0000;
        /// early write
        const UPFRONT_WRITE       = 0b1000_0000_0000_0000;
    }
}

impl Clone for Flags {
    fn clone(&self) -> Self {
        Self(Cell::new(self.0.get()))
    }
}

impl Flags {
    pub(crate) fn new() -> Self {
        Self(Cell::new(FlagsKind::WR_PAUSED))
    }

    pub(crate) fn new_with_upfront_write() -> Self {
        Self(Cell::new(FlagsKind::WR_PAUSED | FlagsKind::UPFRONT_WRITE))
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

    pub(crate) fn is_task_waiting_for_write(&self) -> bool {
        self.contains(FlagsKind::WR_TASK_WAIT)
    }

    pub(crate) fn is_waiting_for_write(&self) -> bool {
        self.intersects(FlagsKind::BUF_W_MUST_FLUSH | FlagsKind::BUF_W_BACKPRESSURE)
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
        self.contains(FlagsKind::HANDLE | FlagsKind::UPFRONT_WRITE | FlagsKind::WR_PAUSED)
    }

    pub(crate) fn is_read_paused(&self) -> bool {
        self.contains(FlagsKind::RD_PAUSED)
    }

    pub(crate) fn is_write_paused(&self) -> bool {
        self.contains(FlagsKind::WR_PAUSED)
    }

    pub(crate) fn is_read_buf_ready(&self) -> bool {
        self.contains(FlagsKind::BUF_R_READY)
    }

    pub(crate) fn is_read_buf_ready_and_full(&self) -> bool {
        self.contains(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL)
    }

    pub(crate) fn is_waiting_for_read(&self) -> bool {
        self.contains(FlagsKind::RD_NOTIFY)
    }

    pub(crate) fn is_wr_backpressure(&self) -> bool {
        self.contains(FlagsKind::BUF_W_BACKPRESSURE)
    }

    pub(crate) fn is_read_paused_or_buf_full(&self) -> bool {
        self.intersects(FlagsKind::RD_PAUSED | FlagsKind::BUF_R_FULL)
    }

    pub(crate) fn set_read_paused(&self) {
        self.insert(FlagsKind::RD_PAUSED);
    }

    pub(crate) fn set_write_paused(&self) {
        self.insert(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn set_io_handle_enabled(&self) {
        self.insert(FlagsKind::HANDLE);
    }

    pub(crate) fn set_task_waiting_for_write(&self) {
        self.insert(FlagsKind::WR_TASK_WAIT);
    }

    pub(crate) fn set_task_waiting_for_write_is_done(&self) {
        self.remove(FlagsKind::WR_TASK_WAIT);
    }

    pub(crate) fn set_wr_backpressure(&self) {
        self.insert(FlagsKind::BUF_W_BACKPRESSURE);
    }

    pub(crate) fn set_filter_stopping(&self) {
        self.insert(FlagsKind::IO_STOPPING_FILTERS);
    }

    pub(crate) fn set_force_closed(&self) {
        self.insert(
            FlagsKind::IO_STOPPED | FlagsKind::IO_STOPPING | FlagsKind::IO_STOPPING_FILTERS,
        );
    }

    pub(crate) fn set_wants_flush(&self) {
        self.insert(FlagsKind::BUF_W_MUST_FLUSH);
    }

    pub(crate) fn set_read_notify(&self) {
        self.insert(FlagsKind::RD_NOTIFY);
    }

    pub(crate) fn set_wants_write(&self) {
        self.insert(FlagsKind::IO_WANTS_WRITE);
    }

    pub(crate) fn set_rd_buf_ready(&self) {
        self.insert(FlagsKind::BUF_R_READY);
    }

    pub(crate) fn set_rd_buf_ready_and_full(&self) {
        self.insert(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL);
    }

    pub(crate) fn set_stopping(&self) {
        self.insert(FlagsKind::IO_STOPPING);
    }

    pub(crate) fn unset_write_paused(&self) {
        self.remove(FlagsKind::WR_PAUSED);
    }

    pub(crate) fn unset_wants_write(&self) {
        self.remove(FlagsKind::IO_WANTS_WRITE);
    }

    pub(crate) fn unset_wr_backpressure(&self) {
        self.remove(FlagsKind::BUF_W_BACKPRESSURE);
    }

    pub(crate) fn unset_flush_and_backpressure(&self) {
        self.remove(FlagsKind::BUF_W_MUST_FLUSH | FlagsKind::BUF_W_BACKPRESSURE);
    }

    pub(crate) fn unset_read_flags(&self) {
        self.remove(FlagsKind::BUF_R_READY | FlagsKind::BUF_R_FULL | FlagsKind::RD_PAUSED);
    }

    pub(crate) fn unset_read_buf_ready(&self) {
        self.remove(FlagsKind::BUF_R_READY);
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
