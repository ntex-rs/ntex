bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    pub struct Flags: u16 {
        /// io is closed
        const IO_STOPPED          = 0b0000_0000_0000_0001;
        /// shutdown io tasks
        const IO_STOPPING         = 0b0000_0000_0000_0010;
        /// shuting down filters
        const IO_STOPPING_FILTERS = 0b0000_0000_0000_0100;
        /// initiate filters shutdown timeout in write task
        const IO_FILTERS_TIMEOUT  = 0b0000_0000_0000_1000;

        /// pause io read
        const RD_PAUSED           = 0b0000_0000_0001_0000;
        /// read any data and notify dispatcher
        const RD_NOTIFY           = 0b0000_0000_1000_0000;

        /// new data is available in read buffer
        const BUF_R_READY         = 0b0000_0000_0010_0000;
        /// read buffer is full
        const BUF_R_FULL          = 0b0000_0000_0100_0000;

        /// wait while write task flushes buf
        const BUF_W_MUST_FLUSH    = 0b0000_0001_0000_0000;

        /// write buffer is full
        const WR_BACKPRESSURE     = 0b0000_0010_0000_0000;
        /// write task paused
        const WR_PAUSED           = 0b0000_0100_0000_0000;

        /// dispatcher is marked stopped
        const DSP_STOP            = 0b0001_0000_0000_0000;
        /// timeout occured
        const DSP_TIMEOUT         = 0b0010_0000_0000_0000;
    }
}

impl Flags {
    pub(crate) fn is_waiting_for_write(&self) -> bool {
        self.intersects(Flags::BUF_W_MUST_FLUSH | Flags::WR_BACKPRESSURE)
    }

    pub(crate) fn waiting_for_write_is_done(&mut self) {
        self.remove(Flags::BUF_W_MUST_FLUSH | Flags::WR_BACKPRESSURE);
    }

    pub(crate) fn is_read_buf_ready(&self) -> bool {
        self.contains(Flags::BUF_R_READY)
    }

    pub(crate) fn cannot_read(self) -> bool {
        self.intersects(Flags::RD_PAUSED | Flags::BUF_R_FULL)
    }

    pub(crate) fn cleanup_read_flags(&mut self) {
        self.remove(Flags::BUF_R_READY | Flags::BUF_R_FULL | Flags::RD_PAUSED);
    }
}
