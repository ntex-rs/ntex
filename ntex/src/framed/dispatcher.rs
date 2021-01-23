//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{
    cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc, time::Duration,
    time::Instant,
};

use either::Either;
use futures::FutureExt;

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use crate::framed::{DispatchItem, ReadTask, State, Timer, WriteTask};
use crate::service::{IntoService, Service};

type Response<U> = <U as Encoder>::Item;

pin_project_lite::pin_project! {
    /// Framed dispatcher - is a future that reads frames from Framed object
    /// and pass then to the service.
    pub struct Dispatcher<S, U>
    where
        S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Encoder,
        U: Decoder,
       <U as Encoder>::Item: 'static,
    {
        service: S,
        inner: DispatcherInner<S, U>,
        #[pin]
        response: Option<S::Future>,
    }
}

struct DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    st: DispatcherState,
    state: State,
    timer: Timer,
    updated: Instant,
    keepalive_timeout: u16,
    shared: Rc<DispatcherShared<S, U>>,
}

struct DispatcherShared<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    codec: U,
    error: Cell<Option<DispatcherError<S::Error, <U as Encoder>::Error>>>,
    inflight: Cell<usize>,
}

#[derive(Copy, Clone, Debug)]
enum DispatcherState {
    Processing,
    Stop,
    Shutdown,
}

pub(crate) enum DispatcherError<S, U> {
    KeepAlive,
    Encoder(U),
    Service(S),
}

impl<S, U> From<Either<S, U>> for DispatcherError<S, U> {
    fn from(err: Either<S, U>) -> Self {
        match err {
            Either::Left(err) => DispatcherError::Service(err),
            Either::Right(err) => DispatcherError::Encoder(err),
        }
    }
}

impl<E1, E2: fmt::Debug> DispatcherError<E1, E2> {
    fn convert<U>(self) -> Option<DispatchItem<U>>
    where
        U: Encoder<Error = E2> + Decoder,
    {
        match self {
            DispatcherError::KeepAlive => Some(DispatchItem::KeepAliveTimeout),
            DispatcherError::Encoder(err) => Some(DispatchItem::EncoderError(err)),
            DispatcherError::Service(_) => None,
        }
    }
}

impl<S, U> Dispatcher<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    /// Construct new `Dispatcher` instance.
    pub fn new<T, F: IntoService<S>>(
        io: T,
        codec: U,
        state: State,
        service: F,
        timer: Timer,
    ) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let io = Rc::new(RefCell::new(io));

        // start support tasks
        crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
        crate::rt::spawn(WriteTask::new(io, state.clone()));

        Self::from_state(codec, state, service, timer)
    }

    /// Construct new `Dispatcher` instance.
    pub fn from_state<F: IntoService<S>>(
        codec: U,
        state: State,
        service: F,
        timer: Timer,
    ) -> Self {
        let updated = timer.now();
        let keepalive_timeout: u16 = 30;

        // register keepalive timer
        let expire = updated + Duration::from_secs(keepalive_timeout as u64);
        timer.register(expire, expire, &state);

        Dispatcher {
            service: service.into_service(),
            response: None,
            inner: DispatcherInner {
                state,
                timer,
                updated,
                keepalive_timeout,
                st: DispatcherState::Processing,
                shared: Rc::new(DispatcherShared {
                    codec,
                    error: Cell::new(None),
                    inflight: Cell::new(0),
                }),
            },
        }
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(mut self, timeout: u16) -> Self {
        // register keepalive timer
        let prev = self.inner.updated
            + Duration::from_secs(self.inner.keepalive_timeout as u64);
        if timeout == 0 {
            self.inner.timer.unregister(prev, &self.inner.state);
        } else {
            let expire = self.inner.updated + Duration::from_secs(timeout as u64);
            self.inner.timer.register(expire, prev, &self.inner.state);
        }
        self.inner.keepalive_timeout = timeout;

        self
    }

    /// Set connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn disconnect_timeout(self, val: u16) -> Self {
        self.inner.state.set_disconnect_timeout(val);
        self
    }
}

impl<S, U> DispatcherShared<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
{
    fn handle_result(
        &self,
        item: Result<S::Response, S::Error>,
        state: &State,
        wake: bool,
    ) {
        self.inflight.set(self.inflight.get() - 1);
        if let Err(err) = state.write_result(item, &self.codec) {
            self.error.set(Some(err.into()));
        }

        if wake {
            state.dsp_wake_task()
        }
    }
}

impl<S, U> DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Decoder + Encoder,
{
    fn take_error(&self) -> Option<DispatchItem<U>> {
        // check for errors
        self.shared
            .error
            .take()
            .and_then(|err| err.convert())
            .or_else(|| self.state.take_io_error().map(DispatchItem::IoError))
    }

    /// check keepalive timeout
    fn check_keepalive(&self) {
        if self.state.is_keepalive() {
            log::trace!("keepalive timeout");
            if let Some(err) = self.shared.error.take() {
                self.shared.error.set(Some(err));
            } else {
                self.shared.error.set(Some(DispatcherError::KeepAlive));
            }
            self.state.dsp_mark_stopped();
        }
    }

    /// update keep-alive timer
    fn update_keepalive(&mut self) {
        if self.keepalive_timeout != 0 {
            let updated = self.timer.now();
            if updated != self.updated {
                let ka = Duration::from_secs(self.keepalive_timeout as u64);
                self.timer
                    .register(updated + ka, self.updated + ka, &self.state);
                self.updated = updated;
            }
        }
    }

    /// unregister keep-alive timer
    fn unregister_keepalive(&self) {
        if self.keepalive_timeout != 0 {
            self.timer.unregister(
                self.updated + Duration::from_secs(self.keepalive_timeout as u64),
                &self.state,
            );
        }
    }
}

impl<S, U> Future for Dispatcher<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        // handle service response future
        if let Some(fut) = this.response.as_mut().as_pin_mut() {
            match fut.poll(cx) {
                Poll::Pending => (),
                Poll::Ready(item) => {
                    this.inner
                        .shared
                        .handle_result(item, &this.inner.state, false);
                    this.response.set(None);
                }
            }
        }

        match this.inner.st {
            DispatcherState::Processing => {
                loop {
                    match this.service.poll_ready(cx) {
                        Poll::Ready(Ok(_)) => {
                            let mut retry = false;

                            // service is ready, wake io read task
                            this.inner.state.dsp_restart_read_task();

                            // check keepalive timeout
                            this.inner.check_keepalive();

                            let item = if this.inner.state.is_dsp_stopped() {
                                log::trace!("dispatcher is instructed to stop");

                                // unregister keep-alive timer
                                this.inner.unregister_keepalive();

                                // check for errors
                                retry = true;
                                this.inner.st = DispatcherState::Stop;
                                this.inner.take_error()
                            } else {
                                // decode incoming bytes stream
                                if this.inner.state.is_read_ready() {
                                    let item = this
                                        .inner
                                        .state
                                        .decode_item(&this.inner.shared.codec);
                                    match item {
                                        Ok(Some(el)) => {
                                            this.inner.update_keepalive();
                                            Some(DispatchItem::Item(el))
                                        }
                                        Ok(None) => {
                                            log::trace!("not enough data to decode next frame, register dispatch task");
                                            this.inner
                                                .state
                                                .dsp_read_more_data(cx.waker());
                                            return Poll::Pending;
                                        }
                                        Err(err) => {
                                            retry = true;
                                            this.inner.st = DispatcherState::Stop;
                                            this.inner.unregister_keepalive();
                                            Some(DispatchItem::DecoderError(err))
                                        }
                                    }
                                } else {
                                    this.inner.state.dsp_register_task(cx.waker());
                                    return Poll::Pending;
                                }
                            };

                            // call service
                            if let Some(item) = item {
                                // optimize first call
                                if this.response.is_none() {
                                    this.response.set(Some(this.service.call(item)));
                                    let res = this
                                        .response
                                        .as_mut()
                                        .as_pin_mut()
                                        .unwrap()
                                        .poll(cx);

                                    if let Poll::Ready(res) = res {
                                        if let Err(err) = this
                                            .inner
                                            .state
                                            .write_result(res, &this.inner.shared.codec)
                                        {
                                            this.inner
                                                .shared
                                                .error
                                                .set(Some(err.into()));
                                        }
                                        this.response.set(None);
                                    } else {
                                        this.inner
                                            .shared
                                            .inflight
                                            .set(this.inner.shared.inflight.get() + 1);
                                    }
                                } else {
                                    this.inner
                                        .shared
                                        .inflight
                                        .set(this.inner.shared.inflight.get() + 1);
                                    let st = this.inner.state.clone();
                                    let shared = this.inner.shared.clone();
                                    crate::rt::spawn(this.service.call(item).map(
                                        move |item| {
                                            shared.handle_result(item, &st, true);
                                        },
                                    ));
                                }
                            }

                            // run again
                            if retry {
                                return self.poll(cx);
                            }
                        }
                        Poll::Pending => {
                            // pause io read task
                            log::trace!("service is not ready, register dispatch task");
                            this.inner.state.dsp_service_not_ready(cx.waker());
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(err)) => {
                            // handle service readiness error
                            log::trace!("service readiness check failed, stopping");
                            this.inner.st = DispatcherState::Stop;
                            this.inner.state.dsp_mark_stopped();
                            this.inner
                                .shared
                                .error
                                .set(Some(DispatcherError::Service(err)));
                            this.inner.unregister_keepalive();
                            return self.poll(cx);
                        }
                    }
                }
            }
            // drain service responses
            DispatcherState::Stop => {
                // service may relay on poll_ready for response results
                let _ = this.service.poll_ready(cx);

                if this.inner.shared.inflight.get() == 0 {
                    this.inner.state.shutdown_io();
                    this.inner.st = DispatcherState::Shutdown;
                    self.poll(cx)
                } else {
                    this.inner.state.dsp_register_task(cx.waker());
                    Poll::Pending
                }
            }
            // shutdown service
            DispatcherState::Shutdown => {
                let err = this.inner.shared.error.take();

                if this.service.poll_shutdown(cx, err.is_some()).is_ready() {
                    log::trace!("service shutdown is completed, stop");

                    Poll::Ready(if let Some(DispatcherError::Service(err)) = err {
                        Err(err)
                    } else {
                        Ok(())
                    })
                } else {
                    this.inner.shared.error.set(err);
                    Poll::Pending
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::FutureExt;

    use crate::codec::BytesCodec;
    use crate::rt::time::delay_for;
    use crate::testing::Io;

    use super::*;

    impl<S, U> Dispatcher<S, U>
    where
        S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Decoder + Encoder + 'static,
        <U as Encoder>::Item: 'static,
    {
        /// Construct new `Dispatcher` instance
        pub(crate) fn debug<T, F: IntoService<S>>(
            io: T,
            codec: U,
            service: F,
        ) -> (Self, State)
        where
            T: AsyncRead + AsyncWrite + Unpin + 'static,
        {
            let timer = Timer::default();
            let keepalive_timeout = 30;
            let updated = timer.now();
            let state = State::new();
            let io = Rc::new(RefCell::new(io));
            let shared = Rc::new(DispatcherShared {
                codec: codec,
                error: Cell::new(None),
                inflight: Cell::new(0),
            });

            crate::rt::spawn(ReadTask::new(io.clone(), state.clone()));
            crate::rt::spawn(WriteTask::new(io.clone(), state.clone()));

            (
                Dispatcher {
                    service: service.into_service(),
                    response: None,
                    inner: DispatcherInner {
                        shared,
                        timer,
                        updated,
                        keepalive_timeout,
                        state: state.clone(),
                        st: DispatcherState::Processing,
                    },
                },
                state,
            )
        }
    }

    #[ntex_rt::test]
    async fn test_basic() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, _) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                delay_for(Duration::from_millis(50)).await;
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        client.close().await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_sink() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, st) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|msg: DispatchItem<BytesCodec>| async move {
                if let DispatchItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        crate::rt::spawn(disp.disconnect_timeout(25).map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(st
            .write_item(Bytes::from_static(b"test"), &mut BytesCodec)
            .is_ok());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        st.close();
        delay_for(Duration::from_millis(200)).await;
        assert!(client.is_server_dropped());
    }

    #[ntex_rt::test]
    async fn test_err_in_service() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let (disp, state) = Dispatcher::debug(
            server,
            BytesCodec,
            crate::fn_service(|_: DispatchItem<BytesCodec>| async move {
                Err::<Option<Bytes>, _>(())
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        state
            .write_item(
                Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"),
                &mut BytesCodec,
            )
            .unwrap();

        let buf = client.read_any();
        assert_eq!(buf, Bytes::from_static(b""));
        delay_for(Duration::from_millis(25)).await;

        // buffer should be flushed
        client.remote_buffer_cap(1024);
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        // write side must be closed, dispatcher waiting for read side to close
        assert!(client.is_closed());

        // close read side
        client.close().await;
        assert!(client.is_server_dropped());
    }
}
