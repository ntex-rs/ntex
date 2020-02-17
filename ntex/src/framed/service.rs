use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, IntoServiceFactory, Service, ServiceFactory};
use either::Either;
use futures::{ready, Stream};
use pin_project::project;

use super::connect::{Connect, ConnectResult};
use super::dispatcher::Dispatcher;
use super::error::ServiceError;

type RequestItem<U> = <U as Decoder>::Item;
type ResponseItem<U> = Option<<U as Encoder>::Item>;

/// Service builder - structure that follows the builder pattern
/// for building instances for framed services.
pub struct Builder<St, C, Io, Codec, Out> {
    connect: C,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, Io, Codec, Out> Builder<St, C, Io, Codec, Out>
where
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    Io: AsyncRead + AsyncWrite,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    /// Construct framed handler service with specified connect service
    pub fn new<F>(connect: F) -> Builder<St, C, Io, Codec, Out>
    where
        F: IntoService<C>,
        Io: AsyncRead + AsyncWrite,
        C: Service<
            Request = Connect<Io, Codec>,
            Response = ConnectResult<Io, St, Codec, Out>,
        >,
        Codec: Decoder + Encoder,
        Out: Stream<Item = <Codec as Encoder>::Item>,
    {
        Builder {
            connect: connect.into_service(),
            _t: PhantomData,
        }
    }

    /// Provide stream items handler service and construct service factory.
    pub fn build<F, T>(self, service: F) -> FramedServiceImpl<St, C, T, Io, Codec, Out>
    where
        F: IntoServiceFactory<T>,
        T: ServiceFactory<
            Config = St,
            Request = RequestItem<Codec>,
            Response = ResponseItem<Codec>,
            Error = C::Error,
            InitError = C::Error,
        >,
    {
        FramedServiceImpl {
            connect: self.connect,
            handler: Rc::new(service.into_factory()),
            _t: PhantomData,
        }
    }
}

/// Service builder - structure that follows the builder pattern
/// for building instances for framed services.
pub struct FactoryBuilder<St, C, Io, Codec, Out> {
    connect: C,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, Io, Codec, Out> FactoryBuilder<St, C, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    /// Construct framed handler new service with specified connect service
    pub fn new<F>(connect: F) -> FactoryBuilder<St, C, Io, Codec, Out>
    where
        F: IntoServiceFactory<C>,
        Io: AsyncRead + AsyncWrite,
        C: ServiceFactory<
            Config = (),
            Request = Connect<Io, Codec>,
            Response = ConnectResult<Io, St, Codec, Out>,
        >,
        Codec: Decoder + Encoder,
        Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
    {
        FactoryBuilder {
            connect: connect.into_factory(),
            _t: PhantomData,
        }
    }

    pub fn build<F, T, Cfg>(
        self,
        service: F,
    ) -> FramedService<St, C, T, Io, Codec, Out, Cfg>
    where
        F: IntoServiceFactory<T>,
        T: ServiceFactory<
            Config = St,
            Request = RequestItem<Codec>,
            Response = ResponseItem<Codec>,
            Error = C::Error,
            InitError = C::Error,
        >,
    {
        FramedService {
            connect: self.connect,
            handler: Rc::new(service.into_factory()),
            _t: PhantomData,
        }
    }
}

pub struct FramedService<St, C, T, Io, Codec, Out, Cfg> {
    connect: C,
    handler: Rc<T>,
    _t: PhantomData<(St, Io, Codec, Out, Cfg)>,
}

impl<St, C, T, Io, Codec, Out, Cfg> ServiceFactory
    for FramedService<St, C, T, Io, Codec, Out, Cfg>
where
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    type Config = Cfg;
    type Request = Io;
    type Response = ();
    type Error = ServiceError<C::Error, Codec>;
    type InitError = C::InitError;
    type Service = FramedServiceImpl<St, C::Service, T, Io, Codec, Out>;
    type Future = FramedServiceResponse<St, C, T, Io, Codec, Out>;

    fn new_service(&self, _: Cfg) -> Self::Future {
        // create connect service and then create service impl
        FramedServiceResponse {
            fut: self.connect.new_service(()),
            handler: self.handler.clone(),
        }
    }
}

#[pin_project::pin_project]
pub struct FramedServiceResponse<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    #[pin]
    fut: C::Future,
    handler: Rc<T>,
}

impl<St, C, T, Io, Codec, Out> Future for FramedServiceResponse<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    type Output =
        Result<FramedServiceImpl<St, C::Service, T, Io, Codec, Out>, C::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let connect = ready!(this.fut.poll(cx))?;

        Poll::Ready(Ok(FramedServiceImpl {
            connect,
            handler: this.handler.clone(),
            _t: PhantomData,
        }))
    }
}

pub struct FramedServiceImpl<St, C, T, Io, Codec, Out> {
    connect: C,
    handler: Rc<T>,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, T, Io, Codec, Out> Service for FramedServiceImpl<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite,
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    type Request = Io;
    type Response = ();
    type Error = ServiceError<C::Error, Codec>;
    type Future = FramedServiceImplResponse<St, Io, Codec, Out, C, T>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(|e| e.into())
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.connect.poll_shutdown(cx, is_error)
    }

    fn call(&mut self, req: Io) -> Self::Future {
        FramedServiceImplResponse {
            inner: FramedServiceImplResponseInner::Connect(
                self.connect.call(Connect::new(req)),
                self.handler.clone(),
            ),
        }
    }
}

#[pin_project::pin_project]
pub struct FramedServiceImplResponse<St, Io, Codec, Out, C, T>
where
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    #[pin]
    inner: FramedServiceImplResponseInner<St, Io, Codec, Out, C, T>,
}

impl<St, Io, Codec, Out, C, T> Future
    for FramedServiceImplResponse<St, Io, Codec, Out, C, T>
where
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    type Output = Result<(), ServiceError<C::Error, Codec>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            match this.inner.poll(cx) {
                Either::Left(new) => {
                    this = self.as_mut().project();
                    this.inner.set(new)
                }
                Either::Right(poll) => return poll,
            };
        }
    }
}

#[pin_project::pin_project]
enum FramedServiceImplResponseInner<St, Io, Codec, Out, C, T>
where
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    Connect(#[pin] C::Future, Rc<T>),
    Handler(#[pin] T::Future, Option<Framed<Io, Codec>>, Option<Out>),
    Dispatcher(Dispatcher<T::Service, Io, Codec, Out>),
}

impl<St, Io, Codec, Out, C, T> FramedServiceImplResponseInner<St, Io, Codec, Out, C, T>
where
    C: Service<
        Request = Connect<Io, Codec>,
        Response = ConnectResult<Io, St, Codec, Out>,
    >,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Error: 'static,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    #[project]
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Either<
        FramedServiceImplResponseInner<St, Io, Codec, Out, C, T>,
        Poll<Result<(), ServiceError<C::Error, Codec>>>,
    > {
        #[project]
        match self.project() {
            FramedServiceImplResponseInner::Connect(fut, handler) => {
                match fut.poll(cx) {
                    Poll::Ready(Ok(res)) => {
                        Either::Left(FramedServiceImplResponseInner::Handler(
                            handler.new_service(res.state),
                            Some(res.framed),
                            res.out,
                        ))
                    }
                    Poll::Pending => Either::Right(Poll::Pending),
                    Poll::Ready(Err(e)) => Either::Right(Poll::Ready(Err(e.into()))),
                }
            }
            FramedServiceImplResponseInner::Handler(fut, framed, out) => {
                match fut.poll(cx) {
                    Poll::Ready(Ok(handler)) => {
                        Either::Left(FramedServiceImplResponseInner::Dispatcher(
                            Dispatcher::new(framed.take().unwrap(), handler, out.take()),
                        ))
                    }
                    Poll::Pending => Either::Right(Poll::Pending),
                    Poll::Ready(Err(e)) => Either::Right(Poll::Ready(Err(e.into()))),
                }
            }
            FramedServiceImplResponseInner::Dispatcher(ref mut fut) => {
                Either::Right(fut.poll(cx))
            }
        }
    }
}
