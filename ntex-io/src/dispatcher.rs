//! Framed transport dispatcher
use std::{
    cell::Cell, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll, time,
};

use ntex_bytes::Pool;
use ntex_codec::{Decoder, Encoder};
use ntex_service::{IntoService, Service};
use ntex_util::time::{now, Seconds};
use ntex_util::{future::Either, spawn};

use super::{DispatchItem, IoBoxed, ReadRef, Timer, WriteRef};

type Response<U> = <U as Encoder>::Item;

pin_project_lite::pin_project! {
    /// Framed dispatcher - is a future that reads frames from bytes stream
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
        fut: Option<S::Future>,
    }
}

struct DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
    U: Encoder + Decoder,
{
    st: Cell<DispatcherState>,
    state: IoBoxed,
    timer: Timer,
    ka_timeout: Seconds,
    ka_updated: Cell<time::Instant>,
    error: Cell<Option<S::Error>>,
    ready_err: Cell<bool>,
    shared: Rc<DispatcherShared<S, U>>,
    pool: Pool,
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
    Backpressure,
    Stop,
    Shutdown,
}

enum DispatcherError<S, U> {
    KeepAlive,
    Encoder(U),
    Service(S),
}

enum PollService<U: Encoder + Decoder> {
    Item(DispatchItem<U>),
    ServiceError,
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
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
    <U as Encoder>::Item: 'static,
{
    /// Construct new `Dispatcher` instance.
    pub fn new<F: IntoService<S>>(
        state: IoBoxed,
        codec: U,
        service: F,
        timer: Timer,
    ) -> Self {
        let updated = now();
        let ka_timeout = Seconds(30);

        // register keepalive timer
        let expire = updated + time::Duration::from(ka_timeout);
        timer.register(expire, expire, &state);

        Dispatcher {
            service: service.into_service(),
            fut: None,
            inner: DispatcherInner {
                pool: state.memory_pool().pool(),
                ka_updated: Cell::new(updated),
                error: Cell::new(None),
                ready_err: Cell::new(false),
                st: Cell::new(DispatcherState::Processing),
                shared: Rc::new(DispatcherShared {
                    codec,
                    error: Cell::new(None),
                    inflight: Cell::new(0),
                }),
                state,
                timer,
                ka_timeout,
            },
        }
    }

    /// Set keep-alive timeout.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default keep-alive timeout is set to 30 seconds.
    pub fn keepalive_timeout(mut self, timeout: Seconds) -> Self {
        // register keepalive timer
        let prev = self.inner.ka_updated.get() + time::Duration::from(self.inner.ka());
        if timeout.is_zero() {
            self.inner.timer.unregister(prev, &self.inner.state);
        } else {
            let expire = self.inner.ka_updated.get() + time::Duration::from(timeout);
            self.inner.timer.register(expire, prev, &self.inner.state);
        }
        self.inner.ka_timeout = timeout;

        self
    }

    /// Set connection disconnect timeout in seconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 1 seconds.
    pub fn disconnect_timeout(self, val: Seconds) -> Self {
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
    fn handle_result(&self, item: Result<S::Response, S::Error>, write: WriteRef<'_>) {
        self.inflight.set(self.inflight.get() - 1);
        match write.encode_result(item, &self.codec) {
            Ok(true) => (),
            Ok(false) => write.enable_backpressure(None),
            Err(err) => self.error.set(Some(err.into())),
        }
        write.wake_dispatcher();
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
        let slf = &this.inner;
        let state = &slf.state;
        let read = state.read();
        let write = state.write();

        // handle service response future
        if let Some(fut) = this.fut.as_mut().as_pin_mut() {
            match fut.poll(cx) {
                Poll::Pending => (),
                Poll::Ready(item) => {
                    this.fut.set(None);
                    slf.shared.inflight.set(slf.shared.inflight.get() - 1);
                    slf.handle_result(item, write);
                }
            }
        }

        // handle memory pool pressure
        if slf.pool.poll_ready(cx).is_pending() {
            read.pause(cx);
            return Poll::Pending;
        }

        loop {
            match slf.st.get() {
                DispatcherState::Processing => {
                    let result = match slf.poll_service(this.service, cx, read) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(result) => result,
                    };

                    let item = match result {
                        PollService::Ready => {
                            if !write.is_ready() {
                                // instruct write task to notify dispatcher when data is flushed
                                write.enable_backpressure(Some(cx));
                                slf.st.set(DispatcherState::Backpressure);
                                DispatchItem::WBackPressureEnabled
                            } else if read.is_ready() {
                                // decode incoming bytes if buffer is ready
                                match read.decode(&slf.shared.codec) {
                                    Ok(Some(el)) => {
                                        slf.update_keepalive();
                                        DispatchItem::Item(el)
                                    }
                                    Ok(None) => {
                                        log::trace!("not enough data to decode next frame, register dispatch task");
                                        read.wake(cx);
                                        return Poll::Pending;
                                    }
                                    Err(err) => {
                                        slf.st.set(DispatcherState::Stop);
                                        slf.unregister_keepalive();
                                        DispatchItem::DecoderError(err)
                                    }
                                }
                            } else {
                                // no new events
                                state.register_dispatcher(cx);
                                return Poll::Pending;
                            }
                        }
                        PollService::Item(item) => item,
                        PollService::ServiceError => continue,
                    };

                    // call service
                    if this.fut.is_none() {
                        // optimize first service call
                        this.fut.set(Some(this.service.call(item)));
                        match this.fut.as_mut().as_pin_mut().unwrap().poll(cx) {
                            Poll::Ready(res) => {
                                this.fut.set(None);
                                slf.handle_result(res, write);
                            }
                            Poll::Pending => {
                                slf.shared.inflight.set(slf.shared.inflight.get() + 1)
                            }
                        }
                    } else {
                        slf.spawn_service_call(this.service.call(item));
                    }
                }
                // handle write back-pressure
                DispatcherState::Backpressure => {
                    let result = match slf.poll_service(this.service, cx, read) {
                        Poll::Ready(result) => result,
                        Poll::Pending => return Poll::Pending,
                    };
                    let item = match result {
                        PollService::Ready => {
                            if write.is_ready() {
                                slf.st.set(DispatcherState::Processing);
                                DispatchItem::WBackPressureDisabled
                            } else {
                                return Poll::Pending;
                            }
                        }
                        PollService::Item(item) => item,
                        PollService::ServiceError => continue,
                    };

                    // call service
                    if this.fut.is_none() {
                        // optimize first service call
                        this.fut.set(Some(this.service.call(item)));
                        match this.fut.as_mut().as_pin_mut().unwrap().poll(cx) {
                            Poll::Ready(res) => {
                                this.fut.set(None);
                                slf.handle_result(res, write);
                            }
                            Poll::Pending => {
                                slf.shared.inflight.set(slf.shared.inflight.get() + 1)
                            }
                        }
                    } else {
                        slf.spawn_service_call(this.service.call(item));
                    }
                }
                // drain service responses
                DispatcherState::Stop => {
                    // service may relay on poll_ready for response results
                    if !this.inner.ready_err.get() {
                        let _ = this.service.poll_ready(cx);
                    }

                    if slf.shared.inflight.get() == 0 {
                        slf.st.set(DispatcherState::Shutdown);
                        state.shutdown(cx);
                    } else {
                        state.register_dispatcher(cx);
                        return Poll::Pending;
                    }
                }
                // shutdown service
                DispatcherState::Shutdown => {
                    let err = slf.error.take();

                    return if this.service.poll_shutdown(cx, err.is_some()).is_ready() {
                        log::trace!("service shutdown is completed, stop");

                        Poll::Ready(if let Some(err) = err {
                            Err(err)
                        } else {
                            Ok(())
                        })
                    } else {
                        slf.error.set(err);
                        Poll::Pending
                    };
                }
            }
        }
    }
}

impl<S, U> DispatcherInner<S, U>
where
    S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>> + 'static,
    U: Decoder + Encoder + 'static,
{
    /// spawn service call
    fn spawn_service_call(&self, fut: S::Future) {
        self.shared.inflight.set(self.shared.inflight.get() + 1);

        let st = self.state.get_ref();
        let shared = self.shared.clone();
        spawn(async move {
            let item = fut.await;
            shared.handle_result(item, st.write());
        });
    }

    fn handle_result(
        &self,
        item: Result<Option<<U as Encoder>::Item>, S::Error>,
        write: WriteRef<'_>,
    ) {
        match write.encode_result(item, &self.shared.codec) {
            Ok(true) => (),
            Ok(false) => write.enable_backpressure(None),
            Err(Either::Left(err)) => {
                self.error.set(Some(err));
            }
            Err(Either::Right(err)) => {
                self.shared.error.set(Some(DispatcherError::Encoder(err)))
            }
        }
    }

    fn poll_service(
        &self,
        srv: &S,
        cx: &mut Context<'_>,
        read: ReadRef<'_>,
    ) -> Poll<PollService<U>> {
        match srv.poll_ready(cx) {
            Poll::Ready(Ok(_)) => {
                // service is ready, wake io read task
                read.resume();

                // check keepalive timeout
                self.check_keepalive();

                // check for errors
                Poll::Ready(if let Some(err) = self.shared.error.take() {
                    log::trace!("error occured, stopping dispatcher");
                    self.unregister_keepalive();
                    self.st.set(DispatcherState::Stop);

                    match err {
                        DispatcherError::KeepAlive => {
                            PollService::Item(DispatchItem::KeepAliveTimeout)
                        }
                        DispatcherError::Encoder(err) => {
                            PollService::Item(DispatchItem::EncoderError(err))
                        }
                        DispatcherError::Service(err) => {
                            self.error.set(Some(err));
                            PollService::ServiceError
                        }
                    }
                } else if self.state.is_dispatcher_stopped() {
                    log::trace!("dispatcher is instructed to stop");

                    self.unregister_keepalive();

                    // process unhandled data
                    if let Ok(Some(el)) = read.decode(&self.shared.codec) {
                        PollService::Item(DispatchItem::Item(el))
                    } else {
                        self.st.set(DispatcherState::Stop);

                        // get io error
                        if let Some(err) = self.state.take_error() {
                            PollService::Item(DispatchItem::IoError(err))
                        } else {
                            PollService::ServiceError
                        }
                    }
                } else {
                    PollService::Ready
                })
            }
            // pause io read task
            Poll::Pending => {
                log::trace!("service is not ready, register dispatch task");
                read.pause(cx);
                Poll::Pending
            }
            // handle service readiness error
            Poll::Ready(Err(err)) => {
                log::trace!("service readiness check failed, stopping");
                self.st.set(DispatcherState::Stop);
                self.error.set(Some(err));
                self.unregister_keepalive();
                self.ready_err.set(true);
                Poll::Ready(PollService::ServiceError)
            }
        }
    }

    fn ka(&self) -> Seconds {
        self.ka_timeout
    }

    fn ka_enabled(&self) -> bool {
        self.ka_timeout.non_zero()
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
        }
    }

    /// update keep-alive timer
    fn update_keepalive(&self) {
        if self.ka_enabled() {
            let updated = now();
            if updated != self.ka_updated.get() {
                let ka = time::Duration::from(self.ka());
                self.timer.register(
                    updated + ka,
                    self.ka_updated.get() + ka,
                    &self.state,
                );
                self.ka_updated.set(updated);
            }
        }
    }

    /// unregister keep-alive timer
    fn unregister_keepalive(&self) {
        if self.ka_enabled() {
            self.timer.unregister(
                self.ka_updated.get() + time::Duration::from(self.ka()),
                &self.state,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;
    use std::sync::{atomic::AtomicBool, atomic::Ordering::Relaxed, Arc, Mutex};
    use std::{cell::RefCell, time::Duration};

    use ntex_bytes::{Bytes, PoolId, PoolRef};
    use ntex_codec::BytesCodec;
    use ntex_util::future::Ready;
    use ntex_util::time::{sleep, Millis};

    use crate::testing::IoTest;
    use crate::{state::Flags, state::IoStateInner, Io, IoStream, WriteRef};

    use super::*;

    pub(crate) struct State(Rc<IoStateInner>);

    impl State {
        fn flags(&self) -> Flags {
            self.0.flags.get()
        }

        fn write(&'_ self) -> WriteRef<'_> {
            WriteRef(self.0.as_ref())
        }

        fn close(&self) {
            self.0.insert_flags(Flags::DSP_STOP);
            self.0.dispatch_task.wake();
        }

        fn set_memory_pool(&self, pool: PoolRef) {
            self.0.pool.set(pool);
        }
    }

    impl<S, U> Dispatcher<S, U>
    where
        S: Service<Request = DispatchItem<U>, Response = Option<Response<U>>>,
        S::Error: 'static,
        S::Future: 'static,
        U: Decoder + Encoder + 'static,
        <U as Encoder>::Item: 'static,
    {
        /// Construct new `Dispatcher` instance
        pub(crate) fn debug<T: IoStream, F: IntoService<S>>(
            io: T,
            codec: U,
            service: F,
        ) -> (Self, State) {
            let state = Io::new(io);
            let timer = Timer::default();
            let ka_timeout = Seconds(1);
            let ka_updated = now();
            let shared = Rc::new(DispatcherShared {
                codec: codec,
                error: Cell::new(None),
                inflight: Cell::new(0),
            });
            let inner = State(state.0 .0.clone());

            let expire = ka_updated + Duration::from_millis(500);
            timer.register(expire, expire, &state);

            (
                Dispatcher {
                    service: service.into_service(),
                    fut: None,
                    inner: DispatcherInner {
                        ka_updated: Cell::new(ka_updated),
                        error: Cell::new(None),
                        ready_err: Cell::new(false),
                        st: Cell::new(DispatcherState::Processing),
                        pool: state.memory_pool().pool(),
                        state: state.into_boxed(),
                        shared,
                        timer,
                        ka_timeout,
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
        spawn(async move {
            let _ = disp.disconnect_timeout(Seconds(1)).await;
        });

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));

        assert!(st
            .write()
            .encode(Bytes::from_static(b"test"), &mut BytesCodec)
            .is_ok());
        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"test"));

        st.close();
        sleep(Millis(1100)).await;
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
            .write()
            .encode(
                Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"),
                &mut BytesCodec,
            )
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
    }

    #[ntex::test]
    async fn test_err_in_service_ready() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(0);
        client.write("GET /test HTTP/1\r\n\r\n");

        let counter = Rc::new(Cell::new(0));

        struct Srv(Rc<Cell<usize>>);

        impl Service for Srv {
            type Request = DispatchItem<BytesCodec>;
            type Response = Option<Response<BytesCodec>>;
            type Error = ();
            type Future = Ready<Option<Response<BytesCodec>>, ()>;

            fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), ()>> {
                self.0.set(self.0.get() + 1);
                Poll::Ready(Err(()))
            }

            fn call(&self, _: DispatchItem<BytesCodec>) -> Self::Future {
                Ready::Ok(None)
            }
        }

        let (disp, state) = Dispatcher::debug(server, BytesCodec, Srv(counter.clone()));
        state
            .write()
            .encode(
                Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"),
                &mut BytesCodec,
            )
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
        assert!(!state.write().is_ready());
        assert_eq!(state.write().with_buf(|buf| buf.len()), 65536);

        client.remote_buffer_cap(10240);
        sleep(Millis(50)).await;
        assert_eq!(state.write().with_buf(|buf| buf.len()), 55296);

        client.remote_buffer_cap(45056);
        sleep(Millis(50)).await;
        assert_eq!(state.write().with_buf(|buf| buf.len()), 10240);

        // backpressure disabled
        assert!(state.write().is_ready());
        assert_eq!(&data.lock().unwrap().borrow()[..], &[0, 1, 2]);
    }

    #[ntex::test]
    async fn test_keepalive() {
        let (client, server) = IoTest::create();
        // do not allow to write to socket
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
        spawn(async move {
            let _ = disp
                .keepalive_timeout(Seconds::ZERO)
                .keepalive_timeout(Seconds(1))
                .await;
        });

        state.0.disconnect_timeout.set(Seconds(1));

        let buf = client.read().await.unwrap();
        assert_eq!(buf, Bytes::from_static(b"GET /test HTTP/1\r\n\r\n"));
        sleep(Millis(3500)).await;

        // write side must be closed, dispatcher should fail with keep-alive
        let flags = state.flags();
        assert!(flags.contains(Flags::IO_SHUTDOWN));
        assert!(flags.contains(Flags::DSP_KEEPALIVE));
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
