use std::{cell::Cell, fmt, future::poll_fn, io, task::ready, task::Context, task::Poll};

use ntex_bytes::{Buf, BufMut, BytesVec};
use ntex_util::{future::lazy, future::select, future::Either, time::sleep, time::Sleep};

use crate::{AsyncRead, AsyncWrite, Flags, IoRef, ReadStatus, WriteStatus};

/// Context for io read task
pub struct ReadContext(IoRef, Cell<Option<Sleep>>);

impl fmt::Debug for ReadContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadContext").field("io", &self.0).finish()
    }
}

impl ReadContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone(), Cell::new(None))
    }

    pub fn clone(&self) -> Self {
        Self(self.0.clone(), Cell::new(None))
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    #[inline]
    /// Io tag
    pub fn io(&self) -> IoRef {
        self.0.clone()
    }

    #[inline]
    /// Check readiness for read operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.0.filter().poll_read_ready(cx)
    }

    /// Wait when io get closed or preparing for close
    pub async fn wait_for_close(&self) {
        poll_fn(|cx| {
            let flags = self.0.flags();

            if flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
                Poll::Ready(())
            } else {
                self.0 .0.read_task.register(cx.waker());
                if flags.contains(Flags::IO_STOPPING_FILTERS) {
                    self.shutdown_filters(cx);
                }
                Poll::Pending
            }
        })
        .await
    }

    #[inline]
    /// Get io error
    pub fn set_stopped(&self, e: Option<io::Error>) {
        self.0 .0.io_stopped(e);
    }

    /// Get read buffer
    pub fn with_buf<F>(&self, f: F) -> Poll<()>
    where
        F: FnOnce(&mut BytesVec) -> Poll<io::Result<usize>>,
    {
        let inner = &self.0 .0;
        let (hw, lw) = self.0.memory_pool().read_params().unpack();
        let result = inner.buffer.with_read_source(&self.0, |buf| {
            // make sure we've got room
            let remaining = buf.remaining_mut();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }

            // call provided callback
            f(buf)
        });

        // handle buffer changes
        match result {
            Poll::Ready(Ok(0)) => {
                inner.io_stopped(None);
                Poll::Ready(())
            }
            Poll::Ready(Ok(nbytes)) => {
                let filter = self.0.filter();
                let _ = filter
                    .process_read_buf(&self.0, &inner.buffer, 0, nbytes)
                    .and_then(|status| {
                        if status.nbytes > 0 {
                            // dest buffer has new data, wake up dispatcher
                            if inner.buffer.read_destination_size() >= hw {
                                log::trace!(
                                    "{}: Io read buffer is too large {}, enable read back-pressure",
                                    self.0.tag(),
                                    nbytes
                                );
                                inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                            } else {
                                inner.insert_flags(Flags::BUF_R_READY);

                                if nbytes >= hw {
                                    // read task is paused because of read back-pressure
                                    // but there is no new data in top most read buffer
                                    // so we need to wake up read task to read more data
                                    // otherwise read task would sleep forever
                                    inner.read_task.wake();
                                }
                            }
                            log::trace!(
                                "{}: New {} bytes available, wakeup dispatcher",
                                self.0.tag(),
                                nbytes
                            );
                            inner.dispatch_task.wake();
                        } else {
                            if nbytes >= hw {
                                // read task is paused because of read back-pressure
                                // but there is no new data in top most read buffer
                                // so we need to wake up read task to read more data
                                // otherwise read task would sleep forever
                                inner.read_task.wake();
                            }
                            if inner.flags.get().contains(Flags::RD_NOTIFY) {
                                // in case of "notify" we must wake up dispatch task
                                // if we read any data from source
                                inner.dispatch_task.wake();
                            }
                        }

                        // while reading, filter wrote some data
                        // in that case filters need to process write buffers
                        // and potentialy wake write task
                        if status.need_write {
                            filter.process_write_buf(&self.0, &inner.buffer, 0)
                        } else {
                            Ok(())
                        }
                    })
                    .map_err(|err| {
                        inner.dispatch_task.wake();
                        inner.io_stopped(Some(err));
                        inner.insert_flags(Flags::BUF_R_READY);
                    });
                Poll::Pending
            }
            Poll::Ready(Err(e)) => {
                inner.io_stopped(Some(e));
                Poll::Ready(())
            }
            Poll::Pending => {
                if inner.flags.get().contains(Flags::IO_STOPPING_FILTERS) {
                    shutdown_filters(&self.0);
                }
                Poll::Pending
            }
        }
    }

    /// Handle read io operations
    pub async fn handle<T>(&self, io: &mut T)
    where
        T: AsyncRead,
    {
        let inner = &self.0 .0;

        loop {
            let result = poll_fn(|cx| self.0.filter().poll_read_ready(cx)).await;
            if result == ReadStatus::Terminate {
                log::trace!("{}: Read task is instructed to shutdown", self.tag());
                break;
            }

            let mut buf = if inner.flags.get().is_read_buf_ready() {
                // read buffer is still not read by dispatcher
                // we cannot touch it
                inner.pool.get().get_read_buf()
            } else {
                inner
                    .buffer
                    .get_read_source()
                    .unwrap_or_else(|| inner.pool.get().get_read_buf())
            };

            // make sure we've got room
            let (hw, lw) = self.0.memory_pool().read_params().unpack();
            let remaining = buf.remaining_mut();
            if remaining <= lw {
                buf.reserve(hw - remaining);
            }
            let total = buf.len();

            // call provided callback
            let (buf, result) = match select(io.read(buf), self.wait_for_close()).await {
                Either::Left(res) => res,
                Either::Right(_) => {
                    log::trace!("{}: Read io is closed, stop read task", self.tag());
                    break;
                }
            };

            // handle incoming data
            let total2 = buf.len();
            let nbytes = total2.saturating_sub(total);
            let total = total2;

            if let Some(mut first_buf) = inner.buffer.get_read_source() {
                first_buf.extend_from_slice(&buf);
                inner.buffer.set_read_source(&self.0, first_buf);
            } else {
                inner.buffer.set_read_source(&self.0, buf);
            }

            // handle buffer changes
            if nbytes > 0 {
                let filter = self.0.filter();
                let res = match filter.process_read_buf(&self.0, &inner.buffer, 0, nbytes) {
                    Ok(status) => {
                        if status.nbytes > 0 {
                            // check read back-pressure
                            if hw < inner.buffer.read_destination_size() {
                                log::trace!(
                                "{}: Io read buffer is too large {}, enable read back-pressure",
                                self.0.tag(),
                                total
                            );
                                inner.insert_flags(Flags::BUF_R_READY | Flags::BUF_R_FULL);
                            } else {
                                inner.insert_flags(Flags::BUF_R_READY);
                            }
                            log::trace!(
                                "{}: New {} bytes available, wakeup dispatcher",
                                self.0.tag(),
                                nbytes
                            );
                            // dest buffer has new data, wake up dispatcher
                            inner.dispatch_task.wake();
                        } else if inner.flags.get().contains(Flags::RD_NOTIFY) {
                            // in case of "notify" we must wake up dispatch task
                            // if we read any data from source
                            inner.dispatch_task.wake();
                        }

                        // while reading, filter wrote some data
                        // in that case filters need to process write buffers
                        // and potentialy wake write task
                        if status.need_write {
                            filter.process_write_buf(&self.0, &inner.buffer, 0)
                        } else {
                            Ok(())
                        }
                    }
                    Err(err) => Err(err),
                };

                if let Err(err) = res {
                    inner.dispatch_task.wake();
                    inner.io_stopped(Some(err));
                    inner.insert_flags(Flags::BUF_R_READY);
                }
            }

            match result {
                Ok(0) => {
                    log::trace!("{}: Tcp stream is disconnected", self.tag());
                    inner.io_stopped(None);
                    break;
                }
                Ok(_) => {
                    if inner.flags.get().contains(Flags::IO_STOPPING_FILTERS) {
                        lazy(|cx| self.shutdown_filters(cx)).await;
                    }
                }
                Err(err) => {
                    log::trace!("{}: Read task failed on io {:?}", self.tag(), err);
                    inner.io_stopped(Some(err));
                    break;
                }
            }
        }
    }

    pub fn shutdown_filters(&self, cx: &mut Context<'_>) {
        let st = &self.0 .0;

        if st.flags.get().contains(Flags::IO_STOPPING_FILTERS) {
            let filter = self.0.filter();

            match filter.shutdown(&self.0, &st.buffer, 0) {
                Ok(Poll::Ready(())) => {
                    st.dispatch_task.wake();
                    st.insert_flags(Flags::IO_STOPPING);
                }
                Ok(Poll::Pending) => {
                    let flags = st.flags.get();

                    // check read buffer, if buffer is not consumed it is unlikely
                    // that filter will properly complete shutdown
                    if flags.contains(Flags::RD_PAUSED)
                        || flags.contains(Flags::BUF_R_FULL | Flags::BUF_R_READY)
                    {
                        st.dispatch_task.wake();
                        st.insert_flags(Flags::IO_STOPPING);
                    } else {
                        // filter shutdown timeout
                        let timeout = self
                            .1
                            .take()
                            .unwrap_or_else(|| sleep(st.disconnect_timeout.get()));
                        if timeout.poll_elapsed(cx).is_ready() {
                            st.dispatch_task.wake();
                            st.insert_flags(Flags::IO_STOPPING);
                        } else {
                            self.1.set(Some(timeout));
                        }
                    }
                }
                Err(err) => {
                    st.io_stopped(Some(err));
                }
            }
            if let Err(err) = filter.process_write_buf(&self.0, &st.buffer, 0) {
                st.io_stopped(Some(err));
            }
        }
    }
}

fn shutdown_filters(io: &IoRef) {
    let st = &io.0;
    let flags = st.flags.get();

    if !flags.intersects(Flags::IO_STOPPED | Flags::IO_STOPPING) {
        let filter = io.filter();
        match filter.shutdown(io, &st.buffer, 0) {
            Ok(Poll::Ready(())) => {
                st.dispatch_task.wake();
                st.insert_flags(Flags::IO_STOPPING);
            }
            Ok(Poll::Pending) => {
                // check read buffer, if buffer is not consumed it is unlikely
                // that filter will properly complete shutdown
                if flags.contains(Flags::RD_PAUSED)
                    || flags.contains(Flags::BUF_R_FULL | Flags::BUF_R_READY)
                {
                    st.dispatch_task.wake();
                    st.insert_flags(Flags::IO_STOPPING);
                }
            }
            Err(err) => {
                st.io_stopped(Some(err));
            }
        }
        if let Err(err) = filter.process_write_buf(io, &st.buffer, 0) {
            st.io_stopped(Some(err));
        }
    }
}

#[derive(Debug)]
/// Context for io write task
pub struct WriteContext(IoRef);

#[derive(Debug)]
/// Context buf for io write task
pub struct WriteContextBuf {
    io: IoRef,
    buf: Option<BytesVec>,
}

impl WriteContext {
    pub(crate) fn new(io: &IoRef) -> Self {
        Self(io.clone())
    }

    pub fn clone(&self) -> Self {
        Self(self.0.clone())
    }

    #[inline]
    /// Io tag
    pub fn tag(&self) -> &'static str {
        self.0.tag()
    }

    /// Check readiness for write operations
    async fn ready(&self) -> WriteStatus {
        poll_fn(|cx| self.0.filter().poll_write_ready(cx)).await
    }

    #[inline]
    /// Check readiness for write operations
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.0.filter().poll_write_ready(cx)
    }

    /// Indicate that write io task is stopped
    fn close(&self, err: Option<io::Error>) {
        self.0 .0.io_stopped(err);
    }

    /// Check if io is closed
    async fn when_stopped(&self) {
        poll_fn(|cx| {
            if self.0.flags().is_stopped() {
                Poll::Ready(())
            } else {
                self.0 .0.write_task.register(cx.waker());
                Poll::Pending
            }
        })
        .await
    }

    /// Wait when io get closed or preparing for close
    pub async fn wait_for_shutdown(&self, flush_buf: bool) {
        let st = &self.0 .0;

        // filter shutdown timeout
        let mut timeout = None;

        poll_fn(|cx| {
            let flags = self.0.flags();

            if flags.intersects(Flags::IO_STOPPING | Flags::IO_STOPPED) {
                Poll::Ready(())
            } else {
                st.write_task.register(cx.waker());
                if flags.contains(Flags::IO_STOPPING_FILTERS) {
                    if timeout.is_none() {
                        timeout = Some(sleep(st.disconnect_timeout.get()));
                    }
                    if timeout.as_ref().unwrap().poll_elapsed(cx).is_ready() {
                        st.dispatch_task.wake();
                        st.insert_flags(Flags::IO_STOPPING);
                        return Poll::Ready(());
                    }
                }
                Poll::Pending
            }
        })
        .await;

        if flush_buf {
            if !self.0.flags().contains(Flags::WR_PAUSED) {
                st.insert_flags(Flags::WR_TASK_WAIT);

                poll_fn(|cx| {
                    let flags = self.0.flags();

                    if flags.intersects(Flags::WR_PAUSED | Flags::IO_STOPPED) {
                        Poll::Ready(())
                    } else {
                        st.write_task.register(cx.waker());

                        if timeout.is_none() {
                            timeout = Some(sleep(st.disconnect_timeout.get()));
                        }
                        if timeout.as_ref().unwrap().poll_elapsed(cx).is_ready() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    }
                })
                .await;
            }
        }
    }

    /// Handle write io operations
    pub async fn handle<T>(&self, io: &mut T)
    where
        T: AsyncWrite,
    {
        let mut buf = WriteContextBuf {
            io: self.0.clone(),
            buf: None,
        };

        loop {
            match self.ready().await {
                WriteStatus::Ready => {
                    // write io stream
                    match select(io.write(&mut buf), self.when_stopped()).await {
                        Either::Left(Ok(_)) => continue,
                        Either::Left(Err(e)) => self.close(Some(e)),
                        Either::Right(_) => return,
                    }
                }
                WriteStatus::Shutdown => {
                    log::trace!("{}: Write task is instructed to shutdown", self.tag());

                    let fut = async {
                        // write io stream
                        io.write(&mut buf).await?;
                        io.flush().await?;
                        io.shutdown().await?;
                        Ok(())
                    };
                    match select(sleep(self.0 .0.disconnect_timeout.get()), fut).await {
                        Either::Left(_) => self.close(None),
                        Either::Right(res) => self.close(res.err()),
                    }
                }
                WriteStatus::Terminate => {
                    log::trace!("{}: Write task is instructed to terminate", self.tag());
                    self.close(io.shutdown().await.err());
                }
            }
            return;
        }
    }

    /// Get write buffer
    pub fn with_buf<F>(&self, f: F) -> Poll<()>
    where
        F: FnOnce(&BytesVec) -> Poll<io::Result<usize>>,
    {
        let inner = &self.0 .0;

        // call provided callback
        let result = inner.buffer.with_write_destination(&self.0, |buf| {
            let buf = if let Some(buf) = buf {
                buf
            } else {
                return Poll::Ready(Ok(0));
            };
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }

            let result = ready!(f(buf));

            match result {
                Ok(n) => {
                    if n == 0 {
                        log::trace!(
                            "{}: Disconnected during flush, written {}",
                            self.tag(),
                            n
                        );
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "failed to write frame to transport",
                        )))
                    } else {
                        if n == buf.len() {
                            buf.clear();
                            Poll::Ready(Ok(0))
                        } else {
                            buf.advance(n);
                            Poll::Ready(Ok(buf.len()))
                        }
                    }
                }
                Err(e) => Poll::Ready(Err(e)),
            }
        });

        let mut flags = inner.flags.get();

        let result = match result {
            Poll::Pending => {
                flags.remove(Flags::WR_PAUSED);
                Poll::Pending
            }
            Poll::Ready(Ok(len)) => {
                // if write buffer is smaller than high watermark value, turn off back-pressure
                if len == 0 {
                    flags.insert(Flags::WR_PAUSED);

                    if flags.is_task_waiting_for_write() {
                        flags.task_waiting_for_write_is_done();
                        inner.write_task.wake();
                    }

                    if flags.is_waiting_for_write() {
                        flags.waiting_for_write_is_done();
                        inner.dispatch_task.wake();
                    }
                    Poll::Ready(())
                } else if flags.contains(Flags::BUF_W_BACKPRESSURE)
                    && len < inner.pool.get().write_params_high() << 1
                {
                    flags.remove(Flags::BUF_W_BACKPRESSURE);
                    inner.dispatch_task.wake();
                    Poll::Pending
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(Err(e)) => {
                self.close(Some(e));
                Poll::Ready(())
            }
        };

        inner.flags.set(flags);
        result
    }
}

impl WriteContextBuf {
    pub fn set(&mut self, mut buf: BytesVec) {
        if buf.is_empty() {
            self.io.memory_pool().release_write_buf(buf);
        } else if let Some(b) = self.buf.take() {
            buf.extend_from_slice(&b);
            self.io.memory_pool().release_write_buf(b);
            self.buf = Some(buf);
        } else if let Some(b) = self.io.0.buffer.set_write_destination(buf) {
            // write buffer is already set
            self.buf = Some(b);
        }

        // if write buffer is smaller than high watermark value, turn off back-pressure
        let inner = &self.io.0;
        let len = self.buf.as_ref().map(|b| b.len()).unwrap_or_default()
            + inner.buffer.write_destination_size();
        let mut flags = inner.flags.get();

        if len == 0 {
            if flags.is_waiting_for_write() {
                flags.waiting_for_write_is_done();
                inner.dispatch_task.wake();
            }
            flags.insert(Flags::WR_PAUSED);
            inner.flags.set(flags);
        } else if flags.contains(Flags::BUF_W_BACKPRESSURE)
            && len < inner.pool.get().write_params_high() << 1
        {
            flags.remove(Flags::BUF_W_BACKPRESSURE);
            inner.flags.set(flags);
            inner.dispatch_task.wake();
        }
    }

    pub fn take(&mut self) -> Option<BytesVec> {
        if let Some(buf) = self.buf.take() {
            Some(buf)
        } else {
            self.io.0.buffer.get_write_destination()
        }
    }
}
