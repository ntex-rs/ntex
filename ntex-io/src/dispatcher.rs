//! Framed transport dispatcher
use std::{cell::Cell, future, pin::Pin, rc::Rc, task::Context, task::Poll, time};

use ntex_bytes::Pool;
use ntex_codec::{Decoder, Encoder};
use ntex_service::{IntoService, Pipeline, Service};
use ntex_util::time::{now, Seconds};
use ntex_util::{future::Either, ready, spawn};

use crate::{Decoded, DispatchItem, IoBoxed, IoStatusUpdate, RecvError};

const ONE_SEC: time::Duration = time::Duration::from_secs(1);

type Response<U> = <U as Encoder>::Item;

#[derive(Clone, Debug)]
/// Shared dispatcher configuration
pub struct DispatcherConfig(Rc<DispatcherConfigInner>);

#[derive(Debug)]
struct DispatcherConfigInner {
    keepalive_timeout: Cell<time::Duration>,
    disconnect_timeout: Cell<Seconds>,
    frame_read_enabled: Cell<bool>,
    frame_read_rate: Cell<u16>,
    frame_read_timeout: Cell<time::Duration>,
    frame_read_max_timeout: Cell<time::Duration>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        DispatcherConfig(Rc::new(DispatcherConfigInner {
            keepalive_timeout: Cell::new(Seconds(30).into()),
            disconnect_timeout: Cell::new(Seconds(1)),
            frame_read_rate: Cell::new(0),
            frame_read_enabled: Cell::new(false),
            frame_read_timeout: Cell::new(Seconds::ZERO.into()),
            frame_read_max_timeout: Cell::new(Seconds::ZERO.into()),
        }))
    }
}

impl DispatcherConfig {
    #[inline]
    /// Get keep-alive timeout
    pub fn keepalive_timeout(&self) -> time::Duration {
        self.0.keepalive_timeout.get()
    }

    #[inline]
    /// Get disconnect timeout
    pub fn disconnect_timeout(&self) -> Seconds {
        self.0.disconnect_timeout.get()
    }

    #[inline]
    /// Get frame read rate
    pub fn frame_read_rate(&self) -> Option<(time::Duration, time::Duration, u16)> {
        if self.0.frame_read_enabled.get() {
            Some((
                self.0.frame_read_timeout.get(),
                self.0.frame_read_max_timeout.get(),
                self.0.frame_read_rate.get(),
            ))
        } else {
            None
        }
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn set_keepalive_timeout(&self, timeout: Seconds) -> &Self {
        self.0.keepalive_timeout.set(timeout.into());
        self
    }

    /// Set connection disconnect timeout.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn set_disconnect_timeout(&self, timeout: Seconds) -> &Self {
        self.0.disconnect_timeout.set(timeout);
        self
    }

    /// Set read rate parameters for single frame.
    ///
    /// Set max timeout for reading single frame. If the client
    /// sends `rate` amount of data, increase the timeout by 1 second for every.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default frame read rate is disabled.
    pub fn set_frame_read_rate(
        &self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> &Self {
        self.0.frame_read_enabled.set(!timeout.is_zero());
        self.0.frame_read_timeout.set(timeout.into());
        self.0.frame_read_max_timeout.set(max_timeout.into());
        self.0.frame_read_rate.set(rate);
        self
    }
}

pin_project_lite::pin_project! {
    /// Dispatcher - is a future that reads frames from bytes stream
    /// and pass then to the service.
    pub struct Dispatcher<S, U>
    where
        S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
        U: Encoder,
        U: Decoder,
    {
        inner: DispatcherInner<S, U>,
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8  {
        const READY_ERR    = 0b0001;
        const IO_ERR       = 0b0010;
        const READ_TIMEOUT = 0b0100;
    }
}

struct DispatcherInner<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    st: DispatcherState,
    error: Option<S::Error>,
    flags: Flags,
    shared: Rc<DispatcherShared<S, U>>,
    pool: Pool,
    cfg: DispatcherConfig,
    read_bytes: u32,
    read_max_timeout: time::Instant,
}

pub(crate) struct DispatcherShared<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    io: IoBoxed,
    codec: U,
    service: Pipeline<S>,
    error: Cell<Option<DispatcherError<S::Error, <U as Encoder>::Error>>>,
    inflight: Cell<usize>,
}

#[derive(Copy, Clone, Debug)]
enum DispatcherState {
    Processing,
    Backpressure,
    Stop,
    Shutdown,
}

#[derive(Debug)]
enum DispatcherError<S, U> {
    Encoder(U),
    Service(S),
}

enum PollService<U: Encoder + Decoder> {
    Item(DispatchItem<U>),
    Continue,
    Ready,
}

impl<S, U> From<Either<S, U>> for DispatcherError<S, U> {
    fn from(err: Either<S, U>) -> Self {
        match err {
            Either::Left(err) => DispatcherError::Service(err),
            Either::Right(err) => DispatcherError::Encoder(err),
        }
    }
}

impl<S, U> Dispatcher<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Decoder + Encoder,
{
    /// Construct new `Dispatcher` instance.
    pub fn with_config<Io, F>(
        io: Io,
        codec: U,
        service: F,
        cfg: &DispatcherConfig,
    ) -> Dispatcher<S, U>
    where
        IoBoxed: From<Io>,
        F: IntoService<S, DispatchItem<U>>,
    {
        let io = IoBoxed::from(io);

        // register keepalive timer
        io.start_timer(cfg.keepalive_timeout());
        io.set_disconnect_timeout(cfg.disconnect_timeout());

        let pool = io.memory_pool().pool();
        let shared = Rc::new(DispatcherShared {
            io,
            codec,
            error: Cell::new(None),
            inflight: Cell::new(0),
            service: Pipeline::new(service.into_service()),
        });

        Dispatcher {
            inner: DispatcherInner {
                pool,
                shared,
                cfg: cfg.clone(),
                error: None,
                flags: Flags::empty(),
                read_bytes: 0,
                read_max_timeout: now(),
                st: DispatcherState::Processing,
            },
        }
    }

    #[doc(hidden)]
    #[deprecated(since = "0.3.6", note = "Use Dispatcher::with_config() method")]
    /// Construct new `Dispatcher` instance.
    pub fn new<Io, F>(io: Io, codec: U, service: F) -> Dispatcher<S, U>
    where
        IoBoxed: From<Io>,
        F: IntoService<S, DispatchItem<U>>,
    {
        Self::with_config(io, codec, service, &DispatcherConfig::default())
    }
}

impl<S, U> Dispatcher<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Decoder + Encoder,
{
    #[doc(hidden)]
    #[deprecated(since = "0.3.6", note = "Use DispatcherConfig methods")]
    /// Set keep-alive timeout.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(self, timeout: Seconds) -> Self {
        // register keepalive timer
        self.inner.shared.io.start_timer(timeout.into());
        self.inner.cfg.set_keepalive_timeout(timeout);
        self
    }

    #[doc(hidden)]
    #[deprecated(since = "0.3.6", note = "Use DispatcherConfig methods")]
    /// Set connection disconnect timeout in seconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn disconnect_timeout(self, val: Seconds) -> Self {
        self.inner.shared.io.set_disconnect_timeout(val);
        self
    }
}

impl<S, U> DispatcherShared<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    fn handle_result(&self, item: Result<S::Response, S::Error>, io: &IoBoxed) {
        self.inflight.set(self.inflight.get() - 1);
        match item {
            Ok(Some(val)) => {
                if let Err(err) = io.encode(val, &self.codec) {
                    self.error.set(Some(DispatcherError::Encoder(err)))
                }
            }
            Err(err) => self.error.set(Some(DispatcherError::Service(err))),
            Ok(None) => (),
        }
        io.wake();
    }
}

impl<S, U> future::Future for Dispatcher<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();
        let slf = &mut this.inner;

        // handle memory pool pressure
        if slf.pool.poll_ready(cx).is_pending() {
            slf.shared.io.pause();
            return Poll::Pending;
        }

        loop {
            match slf.st {
                DispatcherState::Processing => {
                    let item = match ready!(slf.poll_service(cx)) {
                        PollService::Ready => {
                            // decode incoming bytes if buffer is ready
                            match slf.shared.io.poll_recv_decode(&slf.shared.codec, cx) {
                                Ok(decoded) => {
                                    slf.update_timer(&decoded);
                                    if let Some(el) = decoded.item {
                                        DispatchItem::Item(el)
                                    } else {
                                        return Poll::Pending;
                                    }
                                }
                                Err(RecvError::KeepAlive) => {
                                    log::trace!("keep-alive error, stopping dispatcher");
                                    slf.st = DispatcherState::Stop;
                                    if slf.flags.contains(Flags::READ_TIMEOUT) {
                                        DispatchItem::ReadTimeout
                                    } else {
                                        DispatchItem::KeepAliveTimeout
                                    }
                                }
                                Err(RecvError::Stop) => {
                                    log::trace!("dispatcher is instructed to stop");
                                    slf.st = DispatcherState::Stop;
                                    continue;
                                }
                                Err(RecvError::WriteBackpressure) => {
                                    // instruct write task to notify dispatcher when data is flushed
                                    slf.st = DispatcherState::Backpressure;
                                    DispatchItem::WBackPressureEnabled
                                }
                                Err(RecvError::Decoder(err)) => {
                                    log::trace!(
                                        "decoder error, stopping dispatcher: {:?}",
                                        err
                                    );
                                    slf.st = DispatcherState::Stop;
                                    DispatchItem::DecoderError(err)
                                }
                                Err(RecvError::PeerGone(err)) => {
                                    log::trace!(
                                        "peer is gone, stopping dispatcher: {:?}",
                                        err
                                    );
                                    slf.st = DispatcherState::Stop;
                                    DispatchItem::Disconnect(err)
                                }
                            }
                        }
                        PollService::Item(item) => item,
                        PollService::Continue => continue,
                    };

                    // call service
                    let shared = slf.shared.clone();
                    shared.inflight.set(shared.inflight.get() + 1);
                    spawn(async move {
                        let result = shared.service.call(item).await;
                        shared.handle_result(result, &shared.io);
                    });
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    let item = match ready!(slf.poll_service(cx)) {
                        PollService::Ready => {
                            if let Err(err) = ready!(slf.shared.io.poll_flush(cx, false)) {
                                slf.st = DispatcherState::Stop;
                                DispatchItem::Disconnect(Some(err))
                            } else {
                                slf.st = DispatcherState::Processing;
                                DispatchItem::WBackPressureDisabled
                            }
                        }
                        PollService::Item(item) => item,
                        PollService::Continue => continue,
                    };

                    // call service
                    let shared = slf.shared.clone();
                    shared.inflight.set(shared.inflight.get() + 1);
                    spawn(async move {
                        let result = shared.service.call(item).await;
                        shared.handle_result(result, &shared.io);
                    });
                }
                // drain service responses and shutdown io
                DispatcherState::Stop => {
                    slf.shared.io.stop_timer();

                    // service may relay on poll_ready for response results
                    if !slf.flags.contains(Flags::READY_ERR) {
                        let _ = slf.shared.service.poll_ready(cx);
                    }

                    if slf.shared.inflight.get() == 0 {
                        if slf.shared.io.poll_shutdown(cx).is_ready() {
                            slf.st = DispatcherState::Shutdown;
                            continue;
                        }
                    } else if !slf.flags.contains(Flags::IO_ERR) {
                        match ready!(slf.shared.io.poll_status_update(cx)) {
                            IoStatusUpdate::PeerGone(_)
                            | IoStatusUpdate::Stop
                            | IoStatusUpdate::KeepAlive => {
                                slf.flags.insert(Flags::IO_ERR);
                                continue;
                            }
                            IoStatusUpdate::WriteBackpressure => {
                                if ready!(slf.shared.io.poll_flush(cx, true)).is_err() {
                                    slf.flags.insert(Flags::IO_ERR);
                                }
                                continue;
                            }
                        }
                    } else {
                        slf.shared.io.poll_dispatch(cx);
                    }
                    return Poll::Pending;
                }
                // shutdown service
                DispatcherState::Shutdown => {
                    return if slf.shared.service.poll_shutdown(cx).is_ready() {
                        log::trace!("service shutdown is completed, stop");

                        Poll::Ready(if let Some(err) = slf.error.take() {
                            Err(err)
                        } else {
                            Ok(())
                        })
                    } else {
                        Poll::Pending
                    };
                }
            }
        }
    }
}

impl<S, U> DispatcherInner<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
{
    fn poll_service(&mut self, cx: &mut Context<'_>) -> Poll<PollService<U>> {
        match self.shared.service.poll_ready(cx) {
            Poll::Ready(Ok(_)) => {
                // check for errors
                Poll::Ready(if let Some(err) = self.shared.error.take() {
                    log::trace!("error occured, stopping dispatcher");
                    self.st = DispatcherState::Stop;

                    match err {
                        DispatcherError::Encoder(err) => {
                            PollService::Item(DispatchItem::EncoderError(err))
                        }
                        DispatcherError::Service(err) => {
                            self.error = Some(err);
                            PollService::Continue
                        }
                    }
                } else {
                    PollService::Ready
                })
            }
            // pause io read task
            Poll::Pending => {
                log::trace!("service is not ready, register dispatch task");
                match ready!(self.shared.io.poll_read_pause(cx)) {
                    IoStatusUpdate::KeepAlive => {
                        log::trace!("keep-alive error, stopping dispatcher during pause");
                        self.st = DispatcherState::Stop;
                        if self.flags.contains(Flags::READ_TIMEOUT) {
                            Poll::Ready(PollService::Item(DispatchItem::ReadTimeout))
                        } else {
                            Poll::Ready(PollService::Item(DispatchItem::KeepAliveTimeout))
                        }
                    }
                    IoStatusUpdate::Stop => {
                        log::trace!("dispatcher is instructed to stop during pause");
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::Continue)
                    }
                    IoStatusUpdate::PeerGone(err) => {
                        log::trace!(
                            "peer is gone during pause, stopping dispatcher: {:?}",
                            err
                        );
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::Item(DispatchItem::Disconnect(err)))
                    }
                    IoStatusUpdate::WriteBackpressure => Poll::Pending,
                }
            }
            // handle service readiness error
            Poll::Ready(Err(err)) => {
                log::trace!("service readiness check failed, stopping");
                self.st = DispatcherState::Stop;
                self.error = Some(err);
                self.flags.insert(Flags::READY_ERR);
                Poll::Ready(PollService::Continue)
            }
        }
    }

    fn update_timer(&mut self, decoded: &Decoded<<U as Decoder>::Item>) {
        // we got parsed frame
        if decoded.item.is_some() {
            // remove all timers
            if self.flags.contains(Flags::READ_TIMEOUT) {
                self.flags.remove(Flags::READ_TIMEOUT);
            }
            self.shared.io.stop_timer();
        } else if self.flags.contains(Flags::READ_TIMEOUT) {
            // update read timer
            if let Some((_, max, rate)) = self.cfg.frame_read_rate() {
                let bytes = decoded.remains as u32;
                let delta = if bytes > self.read_bytes {
                    (bytes - self.read_bytes).try_into().unwrap_or(u16::MAX)
                } else {
                    bytes.try_into().unwrap_or(u16::MAX)
                };

                // read rate higher than min rate
                if delta >= rate {
                    let n = now();
                    let next = self.shared.io.timer_deadline() + ONE_SEC;
                    let new_timeout = if n >= next { ONE_SEC } else { next - n };

                    // extend timeout
                    if max.is_zero() || (n + new_timeout) <= self.read_max_timeout {
                        self.shared.io.stop_timer();
                        self.shared.io.start_timer(new_timeout);

                        // store current buf size for future rate calculation
                        self.read_bytes = bytes;
                    }
                }
            }
        } else {
            // no new data then start keep-alive timer
            if decoded.remains == 0 {
                self.shared.io.start_timer(self.cfg.keepalive_timeout());
            } else if let Some((period, max, _)) = self.cfg.frame_read_rate() {
                // we got new data but not enough to parse single frame
                // start read timer
                self.flags.insert(Flags::READ_TIMEOUT);

                self.read_bytes = decoded.remains as u32;
                self.shared.io.start_timer(period);
                if !max.is_zero() {
                    self.read_max_timeout = now() + max;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;
    use std::sync::{atomic::AtomicBool, atomic::Ordering::Relaxed, Arc, Mutex};
    use std::{cell::RefCell, io, time::Duration};

    use ntex_bytes::{Bytes, BytesMut, PoolId, PoolRef};
    use ntex_codec::BytesCodec;
    use ntex_service::ServiceCtx;
    use ntex_util::{future::Ready, time::sleep, time::Millis, time::Seconds};

    use super::*;
    use crate::{io::Flags, testing::IoTest, Io, IoRef, IoStream};

    pub(crate) struct State(IoRef);

    impl State {
        fn flags(&self) -> Flags {
            self.0.flags()
        }

        fn io(&self) -> &IoRef {
            &self.0
        }

        fn close(&self) {
            self.0 .0.insert_flags(Flags::DSP_STOP);
            self.0 .0.dispatch_task.wake();
        }

        fn set_memory_pool(&self, pool: PoolRef) {
            self.0 .0.pool.set(pool);
        }
    }

    #[derive(Copy, Clone)]
    struct BCodec(usize);

    impl Encoder for BCodec {
        type Item = Bytes;
        type Error = io::Error;

        fn encode(&self, item: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
            dst.extend_from_slice(&item[..]);
            Ok(())
        }
    }

    impl Decoder for BCodec {
        type Item = BytesMut;
        type Error = io::Error;

        fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            if src.len() < self.0 {
                Ok(None)
            } else {
                Ok(Some(src.split_to(self.0)))
            }
        }
    }

    impl<S, U> Dispatcher<S, U>
    where
        S: Service<DispatchItem<U>, Response = Option<Response<U>>> + 'static,
        U: Decoder + Encoder + 'static,
    {
        /// Construct new `Dispatcher` instance
        pub(crate) fn debug<T: IoStream, F: IntoService<S, DispatchItem<U>>>(
            io: T,
            codec: U,
            service: F,
        ) -> (Self, State) {
            let state = Io::new(io);
            let pool = state.memory_pool().pool();
            let cfg = DispatcherConfig::default()
                .set_keepalive_timeout(Seconds(1))
                .clone();

            let inner = State(state.get_ref());
            state.start_timer(Duration::from_millis(500));

            let shared = Rc::new(DispatcherShared {
                codec,
                io: state.into(),
                error: Cell::new(None),
                inflight: Cell::new(0),
                service: Pipeline::new(service.into_service()),
            });

            (
                Dispatcher {
                    inner: DispatcherInner {
                        error: None,
                        flags: super::Flags::empty(),
                        st: DispatcherState::Processing,
                        read_max_timeout: time::Instant::now(),
                        read_bytes: 0,
                        pool,
                        shared,
                        cfg,
                    },
                },
                inner,
            )
        }
    }

    #[ntex::test]
    async fn test_basic() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, _) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                sleep(Millis(50)).await;
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        spawn(async move {
            let _ = disp.await;
        });

        sleep(Millis(25)).await;
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.write("GET /test HTTP/1\r\n\r\n");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex::test]
    async fn test_sink() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, st) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        #[allow(deprecated)]
        spawn(async move {
            let _ = disp.disconnect_timeout(Seconds(1)).await;
        });

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(st
            .io()
            .encode(Bytes::from_static(b"test"), &BytesCodec)
            .is_ok());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        st.close();
        sleep(Millis(1500)).await;
        assert!(client.is_server_dropped());
    }

    #[ntex::test]
    async fn test_err_in_service() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(|_: DispatchItem<BytesCodec>| async move {
                Err::<Option<Bytes>, _>(())
            }),
        );
        state
            .io()
            .encode(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"), &BytesCodec)
            .unwrap();
        spawn(async move {
            let _ = disp.await;
        });

        // buffer should be flushed
        client.remote_buffer_cap(1024);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        // write side must be closed, dispatcher waiting for read side to close
        assert!(client.is_closed());

        // close read side
        client.close().await;

        // dispatcher is closed
        assert!(client.is_server_dropped());
    }

    #[ntex::test]
    async fn test_err_in_service_ready() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let counter = Rc::new(Cell::new(0));

        struct Srv(Rc<Cell<usize>>);

        impl Service<DispatchItem<BytesCodec>> for Srv {
            type Response = Option<Response<BytesCodec>>;
            type Error = ();
            type Future<'f> = Ready<Option<Response<BytesCodec>>, ()>;

            fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                self.0.set(self.0.get() + 1);
                Poll::Ready(Err(()))
            }

            fn call<'a>(
                &'a self,
                _: DispatchItem<BytesCodec>,
                _: ServiceCtx<'a, Self>,
            ) -> Self::Future<'a> {
                Ready::Ok(None)
            }
        }

        let (disp, state) = Dispatcher::debug(server, BytesCodec, Srv(counter.clone()));
        state
            .io()
            .encode(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"), &BytesCodec)
            .unwrap();
        spawn(async move {
            let _ = disp.await;
        });

        // buffer should be flushed
        client.remote_buffer_cap(1024);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        // write side must be closed, dispatcher waiting for read side to close
        assert!(client.is_closed());

        // close read side
        client.close().await;
        assert!(client.is_server_dropped());

        // service must be checked for readiness only once
        assert_eq!(counter.get(), 1);
    }

    #[ntex::test]
    async fn test_write_backpressure() {
        let (client, server) = IoTest::create();
        // do not allow to write to socket
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(_) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            let bytes = rand::thread_rng()
                                .sample_iter(&rand::distributions::Alphanumeric)
                                .take(65_536)
                                .map(char::from)
                                .collect::<String>();
                            return Ok::<_, ()>(Some(Bytes::from(bytes)));
                        }
                        DispatchItem::WBackPressureEnabled => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        DispatchItem::WBackPressureDisabled => {
                            data.lock().unwrap().borrow_mut().push(2);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        );
        let pool = PoolId::P10.pool_ref();
        pool.set_read_params(8 * 1024, 1024);
        pool.set_write_params(16 * 1024, 1024);
        state.set_memory_pool(pool);

        spawn(async move {
            let _ = disp.await;
        });

        let buf = client.read_any();
        assert_eq!(buf, Bytes::from_static(b""));
        client.write("GET /test HTTP/1\r\n\r\n");
        sleep(Millis(25)).await;

        // buf must be consumed
        assert_eq!(client.remote_buffer(|buf| buf.len()), 0);

        // response message
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 65536);

        client.remote_buffer_cap(10240);
        sleep(Millis(50)).await;
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 55296);

        client.remote_buffer_cap(45056);
        sleep(Millis(50)).await;
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 10240);

        // backpressure disabled
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1, 2]);
    }

    #[ntex::test]
    async fn test_disconnect_during_read_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_util::services::inflight::InFlightService::new(
                1,
                ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| async move {
                    if let DispatchItem::Item(_) = msg {
                        sleep(Millis(500)).await;
                        Ok::<_, ()>(None)
                    } else {
                        Ok(None)
                    }
                }),
            ),
        );
        disp.inner.cfg.set_keepalive_timeout(Seconds::ZERO);
        let pool = PoolId::P10.pool_ref();
        pool.set_read_params(1024, 512);
        state.set_memory_pool(pool);

        let (tx, rx) = ntex::channel::oneshot::channel();
        ntex::rt::spawn(async move {
            let _ = disp.await;
            let _ = tx.send(());
        });

        let bytes = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(1024)
            .map(char::from)
            .collect::<String>();
        client.write(bytes.clone());
        sleep(Millis(25)).await;
        client.write(bytes);
        sleep(Millis(25)).await;

        // close read side
        state.close();
        let _ = rx.recv().await;
    }

    #[ntex::test]
    async fn test_keepalive() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes.freeze()));
                        }
                        DispatchItem::KeepAliveTimeout => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        );
        #[allow(deprecated)]
        spawn(async move {
            let _ = disp
                .keepalive_timeout(Seconds::ZERO)
                .keepalive_timeout(Seconds(1))
                .await;
        });
        state.0 .0.disconnect_timeout.set(Seconds(1));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));
        sleep(Millis(1500)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        let flags = state.flags();
        assert!(flags.contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    #[ntex::test]
    async fn test_read_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let (disp, state) = Dispatcher::debug(
            server,
            BCodec(8),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(bytes) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            return Ok::<_, ()>(Some(bytes.freeze()));
                        }
                        DispatchItem::ReadTimeout => {
                            data.lock().unwrap().borrow_mut().push(1);
                        }
                        _ => (),
                    }
                    Ok(None)
                }
            }),
        );
        spawn(async move {
            disp.inner
                .cfg
                .set_keepalive_timeout(Seconds::ZERO)
                .set_frame_read_rate(Seconds(1), Seconds(2), 2);
            let _ = disp.await;
        });

        client.write("12345678");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"12345678"));

        client.write("1");
        sleep(Millis(500)).await;
        assert!(!state.flags().contains(Flags::IO_STOPPING));
        client.write("23");
        sleep(Millis(500)).await;
        assert!(!state.flags().contains(Flags::IO_STOPPING));
        client.write("4");
        sleep(Millis(1100)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(state.flags().contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    #[ntex::test]
    async fn test_unhandled_data() {
        let handled = Arc::new(AtomicBool::new(false));
        let handled2 = handled.clone();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, _) = Dispatcher::debug(
            server,
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                handled2.store(true, Relaxed);
                async move {
                    sleep(Millis(50)).await;
                    if let DispatchItem::Item(msg) = msg {
                        Ok::<_, ()>(Some(msg.freeze()))
                    } else {
                        panic!()
                    }
                }
            }),
        );
        client.close().await;
        spawn(async move {
            let _ = disp.await;
        });
        sleep(Millis(50)).await;

        assert!(handled.load(Relaxed));
    }
}
