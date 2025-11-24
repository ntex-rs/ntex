//! Framed transport dispatcher
#![allow(clippy::let_underscore_future)]
use std::task::{Context, Poll, ready};
use std::{cell::Cell, future::Future, pin::Pin, rc::Rc};

use ntex_codec::{Decoder, Encoder};
use ntex_service::{IntoService, Pipeline, PipelineBinding, PipelineCall, Service};
use ntex_util::{future::Either, spawn, time::Seconds};

use crate::{Decoded, DispatchItem, IoBoxed, IoStatusUpdate, RecvError};

type Response<U> = <U as Encoder>::Item;

pin_project_lite::pin_project! {
    /// Dispatcher - is a future that reads frames from bytes stream
    /// and pass then to the service.
    pub struct Dispatcher<S, U>
    where
        S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
        U: Encoder,
        U: Decoder,
        U: 'static,
    {
        inner: DispatcherInner<S, U>,
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8  {
        const READY_ERR     = 0b0000001;
        const IO_ERR        = 0b0000010;
        const KA_ENABLED    = 0b0000100;
        const KA_TIMEOUT    = 0b0001000;
        const READ_TIMEOUT  = 0b0010000;
        const IDLE          = 0b0100000;
    }
}

struct DispatcherInner<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder + 'static,
{
    st: DispatcherState,
    error: Option<S::Error>,
    shared: Rc<DispatcherShared<S, U>>,
    response: Option<PipelineCall<S, DispatchItem<U>>>,
    read_remains: u32,
    read_remains_prev: u32,
    read_max_timeout: Seconds,
}

pub(crate) struct DispatcherShared<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    io: IoBoxed,
    codec: U,
    service: PipelineBinding<S, DispatchItem<U>>,
    flags: Cell<Flags>,
    error: Cell<Option<DispatcherError<S::Error, <U as Encoder>::Error>>>,
    inflight: Cell<u32>,
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
    S: Service<DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
{
    /// Construct new `Dispatcher` instance.
    pub fn new<Io, F>(io: Io, codec: U, service: F) -> Dispatcher<S, U>
    where
        IoBoxed: From<Io>,
        F: IntoService<S, DispatchItem<U>>,
    {
        let io = IoBoxed::from(io);
        let flags = if io.cfg().keepalive_timeout().is_zero() {
            Flags::empty()
        } else {
            Flags::KA_ENABLED
        };

        let shared = Rc::new(DispatcherShared {
            io,
            codec,
            flags: Cell::new(flags),
            error: Cell::new(None),
            inflight: Cell::new(0),
            service: Pipeline::new(service.into_service()).bind(),
        });

        Dispatcher {
            inner: DispatcherInner {
                shared,
                response: None,
                error: None,
                read_remains: 0,
                read_remains_prev: 0,
                read_max_timeout: Seconds::ZERO,
                st: DispatcherState::Processing,
            },
        }
    }
}

impl<S, U> DispatcherShared<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    fn handle_result(&self, item: Result<S::Response, S::Error>, io: &IoBoxed, wake: bool) {
        match item {
            Ok(Some(val)) => {
                if let Err(err) = io.encode(val, &self.codec) {
                    self.error.set(Some(DispatcherError::Encoder(err)))
                }
            }
            Err(err) => self.error.set(Some(DispatcherError::Service(err))),
            Ok(None) => (),
        }
        let inflight = self.inflight.get() - 1;
        self.inflight.set(inflight);
        if inflight == 0 {
            self.insert_flags(Flags::IDLE);
        }
        if wake {
            io.wake();
        }
    }

    fn contains(&self, f: Flags) -> bool {
        self.flags.get().intersects(f)
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    fn remove_flags(&self, f: Flags) -> bool {
        let mut flags = self.flags.get();
        if flags.intersects(f) {
            flags.remove(f);
            self.flags.set(flags);
            true
        } else {
            false
        }
    }
}

impl<S, U> Future for Dispatcher<S, U>
where
    S: Service<DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();
        let slf = &mut this.inner;

        // handle service response future
        if let Some(fut) = slf.response.as_mut() {
            if let Poll::Ready(item) = Pin::new(fut).poll(cx) {
                slf.shared.handle_result(item, &slf.shared.io, false);
                slf.response = None;
            }
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
                                    if let Err(err) = slf.handle_timeout() {
                                        slf.st = DispatcherState::Stop;
                                        err
                                    } else {
                                        continue;
                                    }
                                }
                                Err(RecvError::Stop) => {
                                    log::trace!(
                                        "{}: Dispatcher is instructed to stop",
                                        slf.shared.io.tag()
                                    );
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
                                        "{}: Decoder error, stopping dispatcher: {:?}",
                                        slf.shared.io.tag(),
                                        err
                                    );
                                    slf.st = DispatcherState::Stop;
                                    DispatchItem::DecoderError(err)
                                }
                                Err(RecvError::PeerGone(err)) => {
                                    log::trace!(
                                        "{}: Peer is gone, stopping dispatcher: {:?}",
                                        slf.shared.io.tag(),
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

                    slf.call_service(cx, item);
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    match ready!(slf.poll_service(cx)) {
                        PollService::Ready => (),
                        PollService::Item(item) => slf.call_service(cx, item),
                        PollService::Continue => continue,
                    };

                    let item = if let Err(err) = ready!(slf.shared.io.poll_flush(cx, false))
                    {
                        slf.st = DispatcherState::Stop;
                        DispatchItem::Disconnect(Some(err))
                    } else {
                        slf.st = DispatcherState::Processing;
                        DispatchItem::WBackPressureDisabled
                    };
                    slf.call_service(cx, item);
                }
                // drain service responses and shutdown io
                DispatcherState::Stop => {
                    slf.shared.io.stop_timer();

                    // service may relay on poll_ready for response results
                    if !slf.shared.contains(Flags::READY_ERR) {
                        if let Poll::Ready(res) = slf.shared.service.poll_ready(cx) {
                            if res.is_err() {
                                slf.shared.insert_flags(Flags::READY_ERR);
                            }
                        }
                    }

                    if slf.shared.inflight.get() == 0 {
                        if slf.shared.io.poll_shutdown(cx).is_ready() {
                            slf.st = DispatcherState::Shutdown;
                            continue;
                        }
                    } else if !slf.shared.contains(Flags::IO_ERR) {
                        match ready!(slf.shared.io.poll_status_update(cx)) {
                            IoStatusUpdate::PeerGone(_)
                            | IoStatusUpdate::Stop
                            | IoStatusUpdate::KeepAlive => {
                                slf.shared.insert_flags(Flags::IO_ERR);
                                continue;
                            }
                            IoStatusUpdate::WriteBackpressure => {
                                if ready!(slf.shared.io.poll_flush(cx, true)).is_err() {
                                    slf.shared.insert_flags(Flags::IO_ERR);
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
                        log::trace!(
                            "{}: Service shutdown is completed, stop",
                            slf.shared.io.tag()
                        );

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
    fn call_service(&mut self, cx: &mut Context<'_>, item: DispatchItem<U>) {
        let mut fut = self.shared.service.call_nowait(item);
        let inflight = self.shared.inflight.get() + 1;
        self.shared.inflight.set(inflight);
        if inflight == 1 {
            self.shared.remove_flags(Flags::IDLE);
        }

        // optimize first call
        if self.response.is_none() {
            if let Poll::Ready(result) = Pin::new(&mut fut).poll(cx) {
                self.shared.handle_result(result, &self.shared.io, false);
            } else {
                self.response = Some(fut);
            }
        } else {
            let shared = self.shared.clone();
            spawn(async move {
                let result = fut.await;
                shared.handle_result(result, &shared.io, true);
            });
        }
    }

    fn check_error(&mut self) -> PollService<U> {
        // check for errors
        if let Some(err) = self.shared.error.take() {
            log::trace!(
                "{}: Error occured, stopping dispatcher",
                self.shared.io.tag()
            );
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
        }
    }

    fn poll_service(&mut self, cx: &mut Context<'_>) -> Poll<PollService<U>> {
        // wait until service becomes ready
        match self.shared.service.poll_ready(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(self.check_error()),
            // pause io read task
            Poll::Pending => {
                log::trace!(
                    "{}: Service is not ready, register dispatcher",
                    self.shared.io.tag()
                );

                // remove all timers
                self.shared
                    .remove_flags(Flags::KA_TIMEOUT | Flags::READ_TIMEOUT | Flags::IDLE);
                self.shared.io.stop_timer();

                match ready!(self.shared.io.poll_read_pause(cx)) {
                    IoStatusUpdate::KeepAlive => {
                        log::trace!(
                            "{}: Keep-alive error, stopping dispatcher during pause",
                            self.shared.io.tag()
                        );
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::Item(DispatchItem::KeepAliveTimeout))
                    }
                    IoStatusUpdate::Stop => {
                        log::trace!(
                            "{}: Dispatcher is instructed to stop during pause",
                            self.shared.io.tag()
                        );
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::Continue)
                    }
                    IoStatusUpdate::PeerGone(err) => {
                        log::trace!(
                            "{}: Peer is gone during pause, stopping dispatcher: {:?}",
                            self.shared.io.tag(),
                            err
                        );
                        self.st = DispatcherState::Stop;
                        Poll::Ready(PollService::Item(DispatchItem::Disconnect(err)))
                    }
                    IoStatusUpdate::WriteBackpressure => {
                        self.st = DispatcherState::Backpressure;
                        Poll::Ready(PollService::Item(DispatchItem::WBackPressureEnabled))
                    }
                }
            }
            // handle service readiness error
            Poll::Ready(Err(err)) => {
                log::trace!(
                    "{}: Service readiness check failed, stopping",
                    self.shared.io.tag()
                );
                self.st = DispatcherState::Stop;
                self.error = Some(err);
                self.shared.insert_flags(Flags::READY_ERR);
                Poll::Ready(PollService::Item(DispatchItem::Disconnect(None)))
            }
        }
    }

    fn update_timer(&mut self, decoded: &Decoded<<U as Decoder>::Item>) {
        // got parsed frame
        if decoded.item.is_some() {
            self.read_remains = 0;
            self.shared
                .remove_flags(Flags::KA_TIMEOUT | Flags::READ_TIMEOUT | Flags::IDLE);
        } else if self.shared.contains(Flags::READ_TIMEOUT) {
            // received new data but not enough for parsing complete frame
            self.read_remains = decoded.remains as u32;
        } else if self.read_remains == 0 && decoded.remains == 0 {
            // no new data, start keep-alive timer
            if self.shared.contains(Flags::KA_ENABLED)
                && !self.shared.contains(Flags::KA_TIMEOUT)
            {
                log::trace!(
                    "{}: Start keep-alive timer {:?}",
                    self.shared.io.tag(),
                    self.shared.io.cfg().keepalive_timeout()
                );
                self.shared.insert_flags(Flags::KA_TIMEOUT);
                self.shared
                    .io
                    .start_timer(self.shared.io.cfg().keepalive_timeout());
            }
        } else if let Some(params) = self.shared.io.cfg().frame_read_rate() {
            // we got new data but not enough to parse single frame
            // start read timer
            self.shared.insert_flags(Flags::READ_TIMEOUT);

            self.read_remains = decoded.remains as u32;
            self.read_remains_prev = 0;
            self.read_max_timeout = params.max_timeout;
            self.shared.io.start_timer(params.timeout);
        }
    }

    fn handle_timeout(&mut self) -> Result<(), DispatchItem<U>> {
        // check read timer
        if self.shared.contains(Flags::READ_TIMEOUT) {
            if let Some(params) = self.shared.io.cfg().frame_read_rate() {
                let total = self.read_remains - self.read_remains_prev;

                // read rate, start timer for next period
                if total > params.rate {
                    self.read_remains_prev = self.read_remains;
                    self.read_remains = 0;

                    if !params.max_timeout.is_zero() {
                        self.read_max_timeout = Seconds(
                            self.read_max_timeout.0.saturating_sub(params.timeout.0),
                        );
                    }

                    if params.max_timeout.is_zero() || !self.read_max_timeout.is_zero() {
                        log::trace!(
                            "{}: Frame read rate {:?}, extend timer",
                            self.shared.io.tag(),
                            total
                        );
                        self.shared.io.start_timer(params.timeout);
                        return Ok(());
                    }
                    log::trace!(
                        "{}: Max payload timeout has been reached",
                        self.shared.io.tag()
                    );
                }
                Err(DispatchItem::ReadTimeout)
            } else {
                Ok(())
            }
        } else if self.shared.contains(Flags::KA_TIMEOUT | Flags::IDLE) {
            log::trace!(
                "{}: Keep-alive error, stopping dispatcher",
                self.shared.io.tag()
            );
            Err(DispatchItem::KeepAliveTimeout)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::Ordering::Relaxed};
    use std::{cell::RefCell, io};

    use ntex_bytes::{Bytes, BytesMut};
    use ntex_codec::BytesCodec;
    use ntex_service::ServiceCtx;
    use ntex_util::{time::Millis, time::sleep};
    use rand::Rng;

    use super::*;
    use crate::{Flags, Io, IoConfig, IoRef, SharedConfig, testing::IoTest};

    pub(crate) struct State(IoRef);

    impl State {
        fn flags(&self) -> Flags {
            self.0.flags()
        }

        fn io(&self) -> &IoRef {
            &self.0
        }

        fn close(&self) {
            self.0.0.insert_flags(Flags::DSP_STOP);
            self.0.0.dispatch_task.wake();
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
        pub(crate) fn debug(io: Io, codec: U, service: S) -> (Self, State) {
            let flags = if io.cfg().keepalive_timeout().is_zero() {
                super::Flags::empty()
            } else {
                super::Flags::KA_ENABLED
            };

            let inner = State(io.get_ref());
            io.start_timer(Seconds::ONE);

            let shared = Rc::new(DispatcherShared {
                codec,
                io: io.into(),
                flags: Cell::new(flags),
                error: Cell::new(None),
                inflight: Cell::new(0),
                service: Pipeline::new(service).bind(),
            });

            (
                Dispatcher {
                    inner: DispatcherInner {
                        shared,
                        error: None,
                        st: DispatcherState::Processing,
                        response: None,
                        read_remains: 0,
                        read_remains_prev: 0,
                        read_max_timeout: Seconds::ZERO,
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
            Io::from(server),
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

        assert!(format!("{:?}", super::Flags::KA_TIMEOUT.clone()).contains("KA_TIMEOUT"));
    }

    #[ntex::test]
    async fn test_sink() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, st) = Dispatcher::debug(
            Io::from(server),
            BytesCodec,
            ntex_service::fn_service(|msg: DispatchItem<BytesCodec>| async move {
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

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(
            st.io()
                .encode(Bytes::from_static(b"test"), &BytesCodec)
                .is_ok()
        );
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
            Io::from(server),
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
        sleep(Millis(250)).await;
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
            type Error = &'static str;

            async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
                self.0.set(self.0.get() + 1);
                Err("test")
            }

            async fn call(
                &self,
                _: DispatchItem<BytesCodec>,
                _: ServiceCtx<'_, Self>,
            ) -> Result<Self::Response, Self::Error> {
                Ok(None)
            }
        }

        let (disp, state) =
            Dispatcher::debug(Io::from(server), BytesCodec, Srv(counter.clone()));
        spawn(async move {
            let res = disp.await;
            assert_eq!(res, Err("test"));
        });

        state
            .io()
            .encode(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"), &BytesCodec)
            .unwrap();

        // buffer should be flushed
        client.remote_buffer_cap(1024);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        // write side must be closed, dispatcher waiting for read side to close
        sleep(Millis(250)).await;
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

        let io = Io::new(
            server,
            SharedConfig::build("TEST")
                .add(
                    IoConfig::new()
                        .set_read_buf(8 * 1024, 1024, 16)
                        .set_write_buf(16 * 1024, 1024, 16),
                )
                .finish(),
        );

        let (disp, state) = Dispatcher::debug(
            io,
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                let data = data2.clone();
                async move {
                    match msg {
                        DispatchItem::Item(_) => {
                            data.lock().unwrap().borrow_mut().push(0);
                            let bytes = rand::rng()
                                .sample_iter(&rand::distr::Alphanumeric)
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

        client.remote_buffer_cap(48056);
        sleep(Millis(50)).await;
        assert_eq!(state.io().with_write_buf(|buf| buf.len()).unwrap(), 7240);

        // backpressure disabled
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1, 2]);
    }

    #[ntex::test]
    async fn test_disconnect_during_read_backpressure() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);

        let (disp, state) = Dispatcher::debug(
            Io::new(
                server,
                SharedConfig::build("TEST")
                    .add(
                        IoConfig::new()
                            .set_keepalive_timeout(Seconds::ZERO)
                            .set_read_buf(1024, 512, 16),
                    )
                    .finish(),
            ),
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

        let (tx, rx) = ntex::channel::oneshot::channel();
        ntex::rt::spawn(async move {
            let _ = disp.await;
            let _ = tx.send(());
        });

        let bytes = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
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

        let cfg = SharedConfig::build("DBG")
            .add(
                IoConfig::new()
                    .set_disconnect_timeout(Seconds(1))
                    .set_keepalive_timeout(Seconds(1)),
            )
            .finish();

        let (disp, state) = Dispatcher::debug(
            Io::new(server, cfg),
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
        spawn(async move {
            let _ = disp.await;
        });

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        let flags = state.flags();
        assert!(flags.contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    #[ntex::test]
    async fn test_keepalive2() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let cfg = SharedConfig::build("DBG")
            .add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds(1))
                    .set_frame_read_rate(Seconds(1), Seconds(2), 2),
            )
            .finish();

        let (disp, state) = Dispatcher::debug(
            Io::new(server, cfg),
            BCodec(8),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
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
        spawn(async move {
            let _ = disp.await;
        });

        client.write("12345678");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"12345678"));
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        let flags = state.flags();
        assert!(flags.contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    /// Update keep-alive timer after receiving frame
    #[ntex::test]
    async fn test_keepalive3() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let cfg = SharedConfig::build("DBG")
            .add(
                IoConfig::new()
                    .set_keepalive_timeout(Seconds(2))
                    .set_frame_read_rate(Seconds(1), Seconds(2), 2),
            )
            .finish();

        let (disp, _) = Dispatcher::debug(
            Io::new(server, cfg),
            BCodec(1),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
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
        spawn(async move {
            let _ = disp.await;
        });

        client.write("1");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"1"));
        sleep(Millis(750)).await;

        client.write("2");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"2"));

        sleep(Millis(750)).await;
        client.write("3");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"3"));

        sleep(Millis(750)).await;
        assert!(!client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 0, 0]);
    }

    #[ntex::test]
    async fn test_read_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let io = Io::new(
            server,
            SharedConfig::build("TEST")
                .add(
                    IoConfig::new()
                        .set_keepalive_timeout(Seconds::ZERO)
                        .set_frame_read_rate(Seconds(1), Seconds(2), 2),
                )
                .finish(),
        );

        let (disp, state) = Dispatcher::debug(
            io,
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
            let _ = disp.await;
        });

        client.write("12345678");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"12345678"));

        client.write("1");
        sleep(Millis(1000)).await;
        assert!(!state.flags().contains(Flags::IO_STOPPING));
        client.write("23");
        sleep(Millis(1000)).await;
        assert!(!state.flags().contains(Flags::IO_STOPPING));
        client.write("4");
        sleep(Millis(2000)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        assert!(state.flags().contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1]);
    }

    #[ntex::test]
    async fn test_idle_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let data = Arc::new(Mutex::new(RefCell::new(Vec::new())));
        let data2 = data.clone();

        let io = Io::new(
            server,
            SharedConfig::build("DBG")
                .add(IoConfig::new().set_keepalive_timeout(Seconds::ZERO))
                .finish(),
        );
        let ioref = io.get_ref();

        let (disp, state) = Dispatcher::debug(
            io,
            BCodec(1),
            ntex_service::fn_service(move |msg: DispatchItem<BCodec>| {
                let ioref = ioref.clone();
                ntex::rt::spawn(async move {
                    sleep(Millis(500)).await;
                    ioref.notify_timeout();
                });
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
            let _ = disp.await;
        });

        client.write("1");
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"1"));

        sleep(Millis(1000)).await;
        assert!(state.flags().contains(Flags::IO_STOPPING));
        assert!(client.is_closed());
    }

    #[ntex::test]
    async fn test_unhandled_data() {
        let handled = Arc::new(AtomicBool::new(false));
        let handled2 = handled.clone();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, _) = Dispatcher::debug(
            Io::from(server),
            BytesCodec,
            ntex_service::fn_service(move |msg: DispatchItem<BytesCodec>| {
                handled2.store(true, Relaxed);
                async move {
                    sleep(Millis(50)).await;
                    if let DispatchItem::Item(msg) = msg {
                        Ok::<_, ()>(Some(msg.freeze()))
                    } else if let DispatchItem::Disconnect(_) = msg {
                        Ok::<_, ()>(None)
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
