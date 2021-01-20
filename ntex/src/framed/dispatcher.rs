//! Framed transport dispatcher
use std::task::{Context, Poll};
use std::{
    cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc, time::Duration,
    time::Instant,
};

use either::Either;
use futures::FutureExt;

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use crate::framed::{DispatcherItem, FramedReadTask, FramedWriteTask, State, Timer};
use crate::service::{IntoService, Service};

type Response<U> = <U as Encoder>::Item;

pin_project_lite::pin_project! {
    /// Framed dispatcher - is a future that reads frames from Framed object
    /// and pass then to the service.
    pub struct Dispatcher<S, U>
    where
        S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Encoder,
        U: Decoder,
       <U as Encoder>::Item: 'static,
    {
        service: S,
        state: State<U>,
        inner: Rc<DispatcherInner<S, U>>,
        st: DispatcherState,
        timer: Timer<U>,
        updated: Instant,
        keepalive_timeout: u16,
        #[pin]
        response: Option<S::Future>,
    }
}

struct DispatcherInner<S, U>
where
    S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
{
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
    fn convert<U>(self) -> Option<DispatcherItem<U>>
    where
        U: Encoder<Error = E2> + Decoder,
    {
        match self {
            DispatcherError::KeepAlive => Some(DispatcherItem::KeepAliveTimeout),
            DispatcherError::Encoder(err) => Some(DispatcherItem::EncoderError(err)),
            DispatcherError::Service(_) => None,
        }
    }
}

impl<S, U> Dispatcher<S, U>
where
    S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    /// Construct new `Dispatcher` instance.
    pub fn new<T, F: IntoService<S>>(
        io: T,
        state: State<U>,
        service: F,
        timer: Timer<U>,
    ) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + 'static,
    {
        let updated = timer.now();
        let keepalive_timeout: u16 = 30;
        let io = Rc::new(RefCell::new(io));

        // register keepalive timer
        let expire = updated + Duration::from_secs(keepalive_timeout as u64);
        timer.register(expire, expire, &state);

        // start support tasks
        crate::rt::spawn(FramedReadTask::new(io.clone(), state.clone()));
        crate::rt::spawn(FramedWriteTask::new(io, state.clone()));

        let inner = Rc::new(DispatcherInner {
            error: Cell::new(None),
            inflight: Cell::new(0),
        });

        Dispatcher {
            st: DispatcherState::Processing,
            service: service.into_service(),
            response: None,
            state,
            inner,
            timer,
            updated,
            keepalive_timeout,
        }
    }

    /// Set keep-alive timeout in seconds.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(mut self, timeout: u16) -> Self {
        // register keepalive timer
        let prev = self.updated + Duration::from_secs(self.keepalive_timeout as u64);
        if timeout == 0 {
            self.timer.unregister(prev, &self.state);
        } else {
            let expire = self.updated + Duration::from_secs(timeout as u64);
            self.timer.register(expire, prev, &self.state);
        }

        self.keepalive_timeout = timeout;

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
        self.state.set_disconnect_timeout(val);
        self
    }
}

impl<S, U> DispatcherInner<S, U>
where
    S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>>,
    S::Error: 'static,
    S::Future: 'static,
    U: Encoder + Decoder,
    <U as Encoder>::Item: 'static,
{
    fn handle_result(
        &self,
        item: Result<S::Response, S::Error>,
        state: &State<U>,
        wake: bool,
    ) {
        self.inflight.set(self.inflight.get() - 1);
        if let Err(err) = state.write_result(item) {
            self.error.set(Some(err.into()));
        }

        if wake {
            state.dsp_wake_task()
        }
    }
}

impl<S, U> Future for Dispatcher<S, U>
where
    S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    type Output = Result<(), S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        // log::trace!("IO-DISP poll :{:?}:", this.st);

        // handle service response future
        if let Some(fut) = this.response.as_mut().as_pin_mut() {
            match fut.poll(cx) {
                Poll::Pending => (),
                Poll::Ready(item) => {
                    this.inner.handle_result(item, this.state, false);
                    this.response.set(None);
                }
            }
        }

        match this.st {
            DispatcherState::Processing => {
                loop {
                    // log::trace!("IO-DISP state :{:?}:", this.state.get_flags());

                    match this.service.poll_ready(cx) {
                        Poll::Ready(Ok(_)) => {
                            let mut retry = false;

                            // service is ready, wake io read task
                            this.state.dsp_restart_read_task();

                            let item = if this.state.is_dsp_stopped() {
                                log::trace!("dispatcher is instructed to stop");
                                // check keepalive timeout
                                if this.state.is_keepalive_err() {
                                    if let Some(err) = this.inner.error.take() {
                                        this.inner.error.set(Some(err));
                                    } else {
                                        this.inner
                                            .error
                                            .set(Some(DispatcherError::KeepAlive));
                                    }
                                } else if *this.keepalive_timeout != 0 {
                                    // unregister keep-alive timer
                                    this.timer.unregister(
                                        *this.updated
                                            + Duration::from_secs(
                                                *this.keepalive_timeout as u64,
                                            ),
                                        this.state,
                                    );
                                }

                                // check for errors
                                let item = this
                                    .inner
                                    .error
                                    .take()
                                    .and_then(|err| err.convert())
                                    .or_else(|| {
                                        this.state
                                            .take_io_error()
                                            .map(DispatcherItem::IoError)
                                    });
                                *this.st = DispatcherState::Stop;
                                retry = true;

                                item
                            } else {
                                // decode incoming bytes stream

                                if this.state.is_read_ready() {
                                    // this.state.with_read_buf(|buf| {
                                    //     log::trace!(
                                    //         "attempt to decode frame, buffer size is {:?}",
                                    //         buf
                                    //     );
                                    // });

                                    match this.state.decode_item() {
                                        Ok(Some(el)) => {
                                            // update keep-alive timer
                                            if *this.keepalive_timeout != 0 {
                                                let updated = this.timer.now();
                                                if updated != *this.updated {
                                                    let ka = Duration::from_secs(
                                                        *this.keepalive_timeout as u64,
                                                    );
                                                    this.timer.register(
                                                        updated + ka,
                                                        *this.updated + ka,
                                                        this.state,
                                                    );
                                                    *this.updated = updated;
                                                }
                                            }

                                            Some(DispatcherItem::Item(el))
                                        }
                                        Ok(None) => {
                                            // log::trace!("not enough data to decode next frame, register dispatch task");
                                            this.state.dsp_read_more_data(cx.waker());
                                            return Poll::Pending;
                                        }
                                        Err(err) => {
                                            retry = true;
                                            *this.st = DispatcherState::Stop;

                                            // unregister keep-alive timer
                                            if *this.keepalive_timeout != 0 {
                                                this.timer.unregister(
                                                    *this.updated
                                                        + Duration::from_secs(
                                                            *this.keepalive_timeout
                                                                as u64,
                                                        ),
                                                    this.state,
                                                );
                                            }

                                            Some(DispatcherItem::DecoderError(err))
                                        }
                                    }
                                } else {
                                    this.state.dsp_register_task(cx.waker());
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
                                        if let Err(err) = this.state.write_result(res) {
                                            this.inner.error.set(Some(err.into()));
                                        }
                                        this.response.set(None);
                                    } else {
                                        this.inner
                                            .inflight
                                            .set(this.inner.inflight.get() + 1);
                                    }
                                } else {
                                    this.inner
                                        .inflight
                                        .set(this.inner.inflight.get() + 1);
                                    let st = this.state.clone();
                                    let inner = this.inner.clone();
                                    crate::rt::spawn(this.service.call(item).map(
                                        move |item| {
                                            inner.handle_result(item, &st, true);
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
                            this.state.dsp_service_not_ready(cx.waker());
                            return Poll::Pending;
                        }
                        Poll::Ready(Err(err)) => {
                            log::trace!("service readiness check failed, stopping");
                            // service readiness error
                            *this.st = DispatcherState::Stop;
                            this.state.dsp_mark_stopped();
                            this.inner.error.set(Some(DispatcherError::Service(err)));

                            // unregister keep-alive timer
                            if *this.keepalive_timeout != 0 {
                                this.timer.unregister(
                                    *this.updated
                                        + Duration::from_secs(
                                            *this.keepalive_timeout as u64,
                                        ),
                                    this.state,
                                );
                            }

                            return self.poll(cx);
                        }
                    }
                }
            }
            // drain service responses
            DispatcherState::Stop => {
                // service may relay on poll_ready for response results
                let _ = this.service.poll_ready(cx);

                if this.inner.inflight.get() == 0 {
                    this.state.shutdown_io();
                    *this.st = DispatcherState::Shutdown;
                    self.poll(cx)
                } else {
                    this.state.dsp_register_task(cx.waker());
                    Poll::Pending
                }
            }
            // shutdown service
            DispatcherState::Shutdown => {
                let err = this.inner.error.take();

                if this.service.poll_shutdown(cx, err.is_some()).is_ready() {
                    log::trace!("service shutdown is completed, stop");

                    Poll::Ready(if let Some(DispatcherError::Service(err)) = err {
                        Err(err)
                    } else {
                        Ok(())
                    })
                } else {
                    this.inner.error.set(err);
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
        S: Service<Request = DispatcherItem<U>, Response = Option<Response<U>>>,
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
        ) -> (Self, State<U>)
        where
            T: AsyncRead + AsyncWrite + Unpin + 'static,
        {
            let timer = Timer::with(Duration::from_secs(1));
            let keepalive_timeout = 30;
            let updated = timer.now();
            let state = State::new(codec);
            let io = Rc::new(RefCell::new(io));
            let inner = Rc::new(DispatcherInner {
                error: Cell::new(None),
                inflight: Cell::new(0),
            });

            crate::rt::spawn(FramedReadTask::new(io.clone(), state.clone()));
            crate::rt::spawn(FramedWriteTask::new(io.clone(), state.clone()));

            (
                Dispatcher {
                    service: service.into_service(),
                    state: state.clone(),
                    st: DispatcherState::Processing,
                    response: None,
                    timer,
                    updated,
                    keepalive_timeout,
                    inner,
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
            crate::fn_service(|msg: DispatcherItem<BytesCodec>| async move {
                delay_for(Duration::from_millis(50)).await;
                if let DispatcherItem::Item(msg) = msg {
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
            crate::fn_service(|msg: DispatcherItem<BytesCodec>| async move {
                if let DispatcherItem::Item(msg) = msg {
                    Ok::<_, ()>(Some(msg.freeze()))
                } else {
                    panic!()
                }
            }),
        );
        crate::rt::spawn(disp.disconnect_timeout(25).map(|_| ()));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(st.write_item(Bytes::from_static(b"test")).is_ok());
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
            crate::fn_service(|_: DispatcherItem<BytesCodec>| async move {
                Err::<Option<Bytes>, _>(())
            }),
        );
        crate::rt::spawn(disp.map(|_| ()));

        state
            .write_item(Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"))
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
