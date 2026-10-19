use std::task::{Context, Poll};
use std::{future::poll_fn, io};

use ntex_codec::{Decoder, Encoder};
use ntex_util::future::Either;

use crate::{Decoded, IoStatusUpdate, RecvError};

use super::Io;
#[cfg(doc)]
use super::IoRef;

impl<F> Io<F> {
    /// Reads and decodes the next item from the incoming stream.
    ///
    /// Returns `Ok(None)` when the connection closed before another item could
    /// be decoded and nothing was left undecoded, whether the peer
    /// disconnected or the shutdown was started locally.
    ///
    /// If the peer closed its write half while the codec still held a partial
    /// item, the stream was truncated and this returns
    /// [`io::ErrorKind::UnexpectedEof`] in [`Either::Right`] rather than
    /// `Ok(None)`, so that a cut-off frame is not mistaken for a clean end of
    /// stream. Undecodable bytes left after a locally started shutdown are not
    /// treated as truncation.
    ///
    /// Codec errors are returned in [`Either::Left`]. Dispatcher timeouts and
    /// connection errors are returned in [`Either::Right`]; a connection error
    /// may originate from the transport, a filter, or shutdown.
    ///
    /// If write backpressure prevents further reads, this method first waits
    /// for the write buffer to fall below its configured threshold. A
    /// dispatcher timeout that fires during this wait is returned as well.
    pub async fn recv<U>(&self, codec: &U) -> Result<Option<U::Item>, Either<U::Error, io::Error>>
    where
        U: Decoder,
    {
        loop {
            return match poll_fn(|cx| self.poll_recv(codec, cx)).await {
                Ok(item) => Ok(Some(item)),
                Err(RecvError::Timeout) => Err(Either::Right(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timeout",
                ))),
                Err(RecvError::WriteBackpressure) => {
                    let timed_out = poll_fn(|cx| {
                        if self.st().flags.check_dispatcher_timeout() {
                            Poll::Ready(Ok(true))
                        } else {
                            self.poll_flush(cx, false).map_ok(|()| false)
                        }
                    })
                    .await
                    .map_err(Either::Right)?;

                    if timed_out {
                        Err(Either::Right(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Timeout",
                        )))
                    } else {
                        continue;
                    }
                }
                Err(RecvError::Decoder(err)) => Err(Either::Left(err)),
                Err(RecvError::PeerGone(Some(err))) => Err(Either::Right(err)),
                Err(RecvError::PeerGone(None)) => {
                    let st = self.st();
                    if st.flags.is_read_eof() && st.buffer.read_dst_size() != 0 {
                        Err(Either::Right(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "bytes remaining on stream",
                        )))
                    } else {
                        Ok(None)
                    }
                }
            };
        }
    }

    /// Reads exactly enough bytes from this I/O stream to fill `dst`.
    ///
    /// If there is not enough data available, waits for incoming data.
    /// If clean EOF or an error-free shutdown occurs before `dst` is filled,
    /// this returns [`io::ErrorKind::UnexpectedEof`]. Transport errors are
    /// passed through unchanged.
    ///
    /// Each wait goes through [`read_more`](Self::read_more), so this releases
    /// read backpressure unconditionally rather than waiting for the read
    /// buffer to drain to half the high watermark.
    pub async fn read_exact(&self, dst: &mut [u8]) -> io::Result<()> {
        loop {
            let completed = self.with_read_dst(|buf| {
                if buf.len() >= dst.len() {
                    let _ = io::Read::read(buf, dst).expect("Cannot fail");
                    true
                } else {
                    false
                }
            });
            if completed {
                return Ok(());
            }
            // No more bytes will arrive after clean EOF or shutdown.
            if self.read_more().await?.is_none() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Disconnected"));
            }
        }
    }

    #[inline]
    /// Waits until application-facing data is available and allows the
    /// transport to read more data.
    ///
    /// If reads are paused or under backpressure, calling this method resumes
    /// the read task. This is not a passive check of the current buffer, and
    /// read backpressure is released however much data is still buffered.
    ///
    /// Returns `Ok(Some(()))` when input that has not been reported yet is
    /// available, `Ok(None)` when no further input will be reported, and `Err`
    /// if the transport failed. See [`poll_read_more`](Self::poll_read_more)
    /// for what `None` means after a clean EOF.
    pub async fn read_more(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_more(cx)).await
    }

    #[inline]
    /// Waits for the next read from the transport.
    ///
    /// Use this when a filter needs more source bytes. Unlike
    /// [`read_more`](Self::read_more), data already waiting in the application
    /// buffer does not complete this wait. If the read task is paused, this
    /// method wakes it.
    ///
    /// Returns `Some(())` when the transport provides more input. If clean EOF
    /// leaves final data in the application buffer, it returns `Some(())` once
    /// and `None` afterward. A transport error is returned unchanged.
    pub async fn read_notify(&self) -> io::Result<Option<()>> {
        poll_fn(|cx| self.poll_read_notify(cx)).await
    }

    #[inline]
    /// Encodes an item and sends it to the peer, fully flushing the write buffer.
    ///
    /// The flush is bounded by the
    /// [write timeout](crate::IoConfig::set_write_timeout), if one is set;
    /// when it expires this returns [`io::ErrorKind::TimedOut`] in
    /// [`Either::Right`].
    pub async fn send<U>(&self, item: U::Item, codec: &U) -> Result<(), Either<U::Error, io::Error>>
    where
        U: Encoder,
    {
        self.encode(item, codec).map_err(Either::Left)?;

        self.st()
            .with_write_timeout(|cx| self.poll_flush(cx, true))
            .await
            .map_err(Either::Right)?;

        Ok(())
    }

    #[inline]
    /// Wakes the write task and requests a flush of queued output.
    ///
    /// This is the asynchronous counterpart to `poll_flush`. A full flush
    /// completes once all output has reached the peer, including output a
    /// transport has taken ownership of but not yet written. In-flight output
    /// is counted up to 4 GiB, see [`Io::poll_flush`].
    ///
    /// The wait is bounded by the
    /// [write timeout](crate::IoConfig::set_write_timeout), if one is set;
    /// when it expires this returns [`io::ErrorKind::TimedOut`].
    pub async fn flush(&self, full: bool) -> io::Result<()> {
        self.st()
            .with_write_timeout(|cx| self.poll_flush(cx, full))
            .await
    }

    #[inline]
    /// Gracefully shuts down the I/O stream.
    ///
    /// Shutdown runs in two phases, bounded together by a single
    /// [`IoConfig::set_shutdown_timeout`]. First the filters shut down while
    /// both directions stay open, so a filter can emit its closing data and
    /// read the peer's. Then the transport drains the remaining output, pauses
    /// the read side, and closes the connection.
    ///
    /// This completes once the transport backend has finished its shutdown
    /// operation, not merely once the output has been drained.
    ///
    /// If the shutdown deadline expires in either phase, this returns an
    /// [`io::ErrorKind::TimedOut`] error once the transport has stopped.
    ///
    /// [`IoConfig::set_shutdown_timeout`]: crate::IoConfig::set_shutdown_timeout
    pub async fn shutdown(&self) -> io::Result<()> {
        poll_fn(|cx| self.poll_shutdown(cx)).await
    }

    #[inline]
    /// Polls for application-facing data and allows the transport to read more.
    ///
    /// If reads are paused or under backpressure, this resumes the read task.
    /// It therefore changes the read state and should not be used as a passive
    /// buffer check.
    ///
    /// The release is unconditional: unlike consumption through
    /// [`IoRef::decode`], [`IoRef::with_buf`], [`IoRef::with_read_src`] or
    /// [`IoRef::with_read_dst`], which waits for the read buffer to fall to at
    /// most half the high watermark, this releases read backpressure however
    /// much data is still buffered. Asking for more input
    /// is taken as the dispatcher declaring itself able to accept it. This
    /// also resumes reads paused because output produced by reading, for
    /// example replies to peer pings, has not drained yet.
    ///
    /// # Returns
    ///
    /// - `Poll::Pending` while waiting for more data.
    /// - `Poll::Ready(Ok(Some(())))` when input that has not been reported yet
    ///   is available.
    /// - `Poll::Ready(Ok(None))` when no further input will be reported: after
    ///   a clean EOF once the available input has been reported, or when the
    ///   stream closes without an error. Clean EOF leaves the write half open.
    ///
    ///   This reports arrivals, not buffer contents. Input that has already
    ///   been reported stays in the read buffer and remains decodable through
    ///   [`IoRef::decode`] and [`IoRef::with_read_dst`], so `None` does not
    ///   imply that the read buffer is empty.
    /// - `Poll::Ready(Err(e))` if the transport failed.
    pub fn poll_read_more(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();

        if st.flags.is_peer_gone() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(None))
            }
        } else {
            let ready = st.flags.is_read_ready();

            if st.flags.is_read_eof() && !ready {
                return Poll::Ready(Ok(None));
            }

            // If the dispatcher requests more data but no read occurs,
            // restart the read task.
            if st.flags.is_read_paused_or_backpressure() || st.flags.is_read_wr_backpressure() {
                st.unset_read_ready_and_backpressure();
                st.flags.unset_read_paused();
                st.flags.unset_read_wr_backpressure();
                st.wake_read_task();
                if ready {
                    Poll::Ready(Ok(Some(())))
                } else {
                    st.dispatch_task.register(cx.waker());
                    Poll::Pending
                }
            } else if ready {
                Poll::Ready(Ok(Some(())))
            } else {
                st.dispatch_task.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[inline]
    /// Polls for the next read from the transport.
    ///
    /// This is the polling version of [`read_notify`](Self::read_notify).
    /// Existing application data does not make it ready. When another transport
    /// read is needed, this wakes the read task and registers the current waker.
    ///
    /// `Some(())` means that more input arrived. Clean EOF may produce one last
    /// `Some(())` when filters leave final application data; later polls return
    /// `None`. Transport errors are returned unchanged.
    ///
    /// Paused or back-pressured reads are resumed through
    /// [`poll_read_more`](Self::poll_read_more), which releases read
    /// backpressure unconditionally.
    pub fn poll_read_notify(&self, cx: &mut Context<'_>) -> Poll<io::Result<Option<()>>> {
        let st = self.st();
        if st.flags.is_stopping_or_terminating() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(None))
            }
        } else if st.flags.is_read_eof() {
            let notified = st.flags.take_read_notified();
            if notified && st.flags.is_read_ready() {
                Poll::Ready(Ok(Some(())))
            } else {
                Poll::Ready(Ok(None))
            }
        } else if st.flags.take_read_notified() {
            Poll::Ready(Ok(Some(())))
        } else {
            st.flags.set_read_notify();
            // Resumes the read task if reads are paused. The result is
            // discarded on purpose: buffered application data does not
            // complete this wait, and the eof and closed cases are handled
            // by the branches above.
            let _ = self.poll_read_more(cx);
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Decodes the next item from the incoming byte stream.
    ///
    /// Returns `Poll::Pending` when the codec needs more input, after going
    /// through [`poll_read_more`](Self::poll_read_more), which wakes the read
    /// task and releases read backpressure unconditionally.
    ///
    /// An error return does not register the waker.
    pub fn poll_recv<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Poll<Result<U::Item, RecvError<U>>>
    where
        U: Decoder,
    {
        let decoded = self.poll_recv_decode(codec, cx)?;

        if let Some(item) = decoded.item {
            Poll::Ready(Ok(item))
        } else {
            Poll::Pending
        }
    }

    #[inline]
    /// Attempts to decode an item and reports buffer progress.
    ///
    /// `Decoded::consumed` is the number of bytes consumed by this decode
    /// attempt and `Decoded::remains` is the number left in the
    /// application-facing read buffer. If the codec needs more input, this
    /// returns `Ok` with `item` set to `None` after arranging for `cx` to be
    /// woken when progress is possible.
    ///
    /// An error return does not register the waker. While the connection is
    /// open, an expired dispatcher timer and then active write backpressure are
    /// reported before the read buffer is decoded, so an error never follows a
    /// decode attempt and `Decoded` is never lost. The caller decides whether
    /// input received together with a timeout is decoded first, see
    /// [`IoRef::decode_item`]. Once the connection
    /// is closing neither is reported, the buffered input is decoded and
    /// `RecvError::PeerGone` is returned when no item is left.
    ///
    /// When the codec needs more input this goes through
    /// [`poll_read_more`](Self::poll_read_more), which releases read
    /// backpressure unconditionally.
    pub fn poll_recv_decode<U>(
        &self,
        codec: &U,
        cx: &mut Context<'_>,
    ) -> Result<Decoded<U::Item>, RecvError<U>>
    where
        U: Decoder,
    {
        let st = self.st();
        st.flags.unset_read_ready();

        let closed = st.flags.is_stopping() || st.flags.is_terminating();
        if !closed {
            if st.flags.check_dispatcher_timeout() {
                return Err(RecvError::Timeout);
            } else if st.flags.is_wr_backpressure() {
                return Err(RecvError::WriteBackpressure);
            }
        }

        let decoded = self
            .decode_item(codec)
            .map_err(|err| RecvError::Decoder(err))?;

        if decoded.item.is_some() {
            Ok(decoded)
        } else if st.flags.is_stopping() || st.flags.is_terminating() {
            Err(RecvError::PeerGone(st.error()))
        } else {
            match self.poll_read_more(cx) {
                Poll::Pending | Poll::Ready(Ok(Some(()))) => {
                    #[cfg(feature = "trace")]
                    if decoded.remains != 0 {
                        log::trace!("{}: Not enough data to decode next frame", self.tag());
                    }
                    Ok(decoded)
                }
                Poll::Ready(Err(e)) => Err(RecvError::PeerGone(Some(e))),
                Poll::Ready(Ok(None)) => Err(RecvError::PeerGone(None)),
            }
        }
    }

    #[inline]
    /// Wakes the write task and instructs it to flush data.
    ///
    /// A full flush waits until all output has reached the peer.
    ///
    /// Otherwise this returns immediately while the outstanding size is below
    /// the configured high watermark. Reaching that watermark enables write
    /// backpressure, and the call then waits until the outstanding size falls
    /// to half of it.
    ///
    /// Output that a completion based transport has taken ownership of counts
    /// as outstanding until it reaches the peer, so a full flush does not
    /// complete while a write is still in flight.
    ///
    /// In-flight output is counted up to 4 GiB (`u32::MAX` bytes). If more
    /// than that is in flight at once, a full flush may complete before all
    /// of it reaches the peer. Respecting write backpressure keeps the
    /// outstanding size far below this limit.
    pub fn poll_flush(&self, cx: &mut Context<'_>, full: bool) -> Poll<io::Result<()>> {
        let st = self.st();

        // flush filter state
        st.buffer.process_write_buf_force(self)?;
        self.consolidate_write_state(false)?;

        let len = st.write_outstanding();
        if len > 0 {
            if st.flags.is_peer_gone() {
                return Poll::Ready(Err(st.error_or_disconnected()));
            } else if full {
                st.flags.set_wants_write_flush();
                st.dispatch_task.register(cx.waker());
                return Poll::Pending;
            } else if st.flags.is_wr_backpressure() {
                if !st.should_disable_wr_backpressure(len) {
                    st.dispatch_task.register(cx.waker());
                    return Poll::Pending;
                }
            } else if st.is_wr_backpressure_needed(len) {
                st.set_wr_backpressure();
                st.dispatch_task.register(cx.waker());
                return Poll::Pending;
            }
        }
        if st.flags.is_peer_gone() && !st.flags.is_write_flush() {
            Poll::Ready(Err(st.error_or_disconnected()))
        } else {
            st.unset_wr_backpressure_and_flush();
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    /// Polls graceful shutdown through transport completion.
    ///
    /// `Poll::Ready` is returned only after the transport backend marks the
    /// connection stopped, not merely when filter shutdown and flushing finish.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let st = self.st();

        if st.flags.is_closed() {
            if let Some(err) = st.error() {
                Poll::Ready(Err(err))
            } else {
                Poll::Ready(Ok(()))
            }
        } else {
            if !st.flags.is_terminating() && !st.flags.is_stopping_filters() {
                st.start_shutdown();
            }
            // Reads must keep running during shutdown so that the transport can
            // observe a peer EOF and filters can complete their shutdown
            // handshake. `BUF_R_READY` is deliberately left alone: it marks
            // input the dispatcher has not consumed yet.
            //
            // Only the pause is cleared. Backpressure means the read buffer is
            // full, which the shutdown must still respect: clearing it on every
            // poll would let a peer that keeps sending grow the buffer without
            // bound, and would hide the blocked shutdown detection in
            // `poll_filters_shutdown`, which tests for exactly that flag.
            //
            // The tasks are woken only when this changes their state:
            // `start_shutdown` wakes both. Waking them on every poll would let
            // a caller that is polled spuriously keep the runtime busy with
            // wakeups; compio then never polls for I/O completions.
            if st.flags.is_read_paused() {
                st.resume_read_task();
            }
            st.dispatch_task.register(cx.waker());
            Poll::Pending
        }
    }

    #[inline]
    /// Pauses the read task and polls for a status update.
    ///
    /// The transport stops reading until the pause is cancelled. There is no
    /// explicit resume: the pause is cancelled implicitly by any operation that
    /// touches the read buffer or asks for more input, namely
    /// [`read_more`](Self::read_more), [`poll_read_more`](Self::poll_read_more),
    /// [`IoRef::decode`], [`IoRef::decode_item`], and [`IoRef::with_read_dst`].
    /// Releasing read backpressure cancels it as well. Because those methods
    /// are available through every [`IoRef`] clone, the pause holds only while
    /// no other holder touches the read buffer.
    ///
    /// See [`poll_status_update`](Self::poll_status_update) for the reported
    /// updates.
    pub fn poll_read_pause(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        if !st.flags.is_read_paused() {
            st.wake_read_task();
            st.flags.set_read_paused();
        }
        self.poll_status_update(cx)
    }

    #[inline]
    /// Polls for available status updates.
    ///
    /// `Timeout` consumes the pending dispatcher-timeout notification.
    /// `WriteBackpressure` is reported while backpressure is active. The poll
    /// that observes the write buffer falling below its release threshold
    /// releases backpressure and reports no status update, matching
    /// [`poll_flush`](Self::poll_flush). `PeerGone` is returned once the
    /// connection has closed, whether the peer disconnected, the transport
    /// failed, or the shutdown was started locally.
    pub fn poll_status_update(&self, cx: &mut Context<'_>) -> Poll<IoStatusUpdate> {
        let st = self.st();
        st.dispatch_task.register(cx.waker());
        if st.flags.is_peer_gone() {
            Poll::Ready(IoStatusUpdate::PeerGone(st.error()))
        } else if st.flags.check_dispatcher_timeout() {
            Poll::Ready(IoStatusUpdate::Timeout)
        } else if st.flags.is_wr_backpressure() {
            // write backpressure is enabled and outstanding output is smaller than half
            if st.should_disable_wr_backpressure(st.write_outstanding()) {
                st.unset_wr_backpressure();
                Poll::Pending
            } else {
                Poll::Ready(IoStatusUpdate::WriteBackpressure)
            }
        } else {
            Poll::Pending
        }
    }

    #[inline]
    /// Registers a dispatch task.
    pub fn register_dispatch(&self, cx: &mut Context<'_>) {
        self.st().dispatch_task.register(cx.waker());
    }
}
