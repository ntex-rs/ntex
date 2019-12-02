use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use actix_service::{IntoService, IntoServiceFactory, Service, ServiceFactory};
use either::Either;
use futures::future::{FutureExt, LocalBoxFuture};
use pin_project::project;

use crate::connect::{Connect, ConnectResult};
use crate::dispatcher::FramedDispatcher;
use crate::error::ServiceError;
use crate::item::Item;

type RequestItem<S, U> = Item<S, U>;
type ResponseItem<U> = Option<<U as Encoder>::Item>;

/// Service builder - structure that follows the builder pattern
/// for building instances for framed services.
pub struct Builder<St, Codec>(PhantomData<(St, Codec)>);

impl<St: Clone, Codec> Default for Builder<St, Codec> {
    fn default() -> Builder<St, Codec> {
        Builder::new()
    }
}

impl<St: Clone, Codec> Builder<St, Codec> {
    pub fn new() -> Builder<St, Codec> {
        Builder(PhantomData)
    }

    /// Construct framed handler service with specified connect service
    pub fn service<Io, C, F>(self, connect: F) -> ServiceBuilder<St, C, Io, Codec>
    where
        F: IntoService<C>,
        Io: AsyncRead + AsyncWrite,
        C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
        Codec: Decoder + Encoder,
    {
        ServiceBuilder {
            connect: connect.into_service(),
            disconnect: None,
            _t: PhantomData,
        }
    }

    /// Construct framed handler new service with specified connect service
    pub fn factory<Io, C, F>(self, connect: F) -> NewServiceBuilder<St, C, Io, Codec>
    where
        F: IntoServiceFactory<C>,
        Io: AsyncRead + AsyncWrite,
        C: ServiceFactory<
            Config = (),
            Request = Connect<Io>,
            Response = ConnectResult<Io, St, Codec>,
        >,
        C::Error: 'static,
        C::Future: 'static,
        Codec: Decoder + Encoder,
    {
        NewServiceBuilder {
            connect: connect.into_factory(),
            disconnect: None,
            _t: PhantomData,
        }
    }
}

pub struct ServiceBuilder<St, C, Io, Codec> {
    connect: C,
    disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
    _t: PhantomData<(St, Io, Codec)>,
}

impl<St, C, Io, Codec> ServiceBuilder<St, C, Io, Codec>
where
    St: Clone,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    /// Callback to execute on disconnect
    ///
    /// Second parameter indicates error occured during disconnect.
    pub fn disconnect<F, Out>(mut self, disconnect: F) -> Self
    where
        F: Fn(&mut St, bool) + 'static,
    {
        self.disconnect = Some(Rc::new(disconnect));
        self
    }

    /// Provide stream items handler service and construct service factory.
    pub fn finish<F, T>(self, service: F) -> FramedServiceImpl<St, C, T, Io, Codec>
    where
        F: IntoServiceFactory<T>,
        T: ServiceFactory<
                Config = St,
                Request = RequestItem<St, Codec>,
                Response = ResponseItem<Codec>,
                Error = C::Error,
                InitError = C::Error,
            > + 'static,
    {
        FramedServiceImpl {
            connect: self.connect,
            handler: Rc::new(service.into_factory()),
            disconnect: self.disconnect.clone(),
            _t: PhantomData,
        }
    }
}

pub struct NewServiceBuilder<St, C, Io, Codec> {
    connect: C,
    disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
    _t: PhantomData<(St, Io, Codec)>,
}

impl<St, C, Io, Codec> NewServiceBuilder<St, C, Io, Codec>
where
    St: Clone,
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io>,
        Response = ConnectResult<Io, St, Codec>,
    >,
    C::Error: 'static,
    C::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    /// Callback to execute on disconnect
    ///
    /// Second parameter indicates error occured during disconnect.
    pub fn disconnect<F>(mut self, disconnect: F) -> Self
    where
        F: Fn(&mut St, bool) + 'static,
    {
        self.disconnect = Some(Rc::new(disconnect));
        self
    }

    pub fn finish<F, T, Cfg>(self, service: F) -> FramedService<St, C, T, Io, Codec, Cfg>
    where
        F: IntoServiceFactory<T>,
        T: ServiceFactory<
                Config = St,
                Request = RequestItem<St, Codec>,
                Response = ResponseItem<Codec>,
                Error = C::Error,
                InitError = C::Error,
            > + 'static,
    {
        FramedService {
            connect: self.connect,
            handler: Rc::new(service.into_factory()),
            disconnect: self.disconnect,
            _t: PhantomData,
        }
    }
}

pub struct FramedService<St, C, T, Io, Codec, Cfg> {
    connect: C,
    handler: Rc<T>,
    disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
    _t: PhantomData<(St, Io, Codec, Cfg)>,
}

impl<St, C, T, Io, Codec, Cfg> ServiceFactory for FramedService<St, C, T, Io, Codec, Cfg>
where
    St: Clone + 'static,
    Io: AsyncRead + AsyncWrite,
    C: ServiceFactory<
        Config = (),
        Request = Connect<Io>,
        Response = ConnectResult<Io, St, Codec>,
    >,
    C::Error: 'static,
    C::Future: 'static,
    T: ServiceFactory<
            Config = St,
            Request = RequestItem<St, Codec>,
            Response = ResponseItem<Codec>,
            Error = C::Error,
            InitError = C::Error,
        > + 'static,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    type Config = Cfg;
    type Request = Io;
    type Response = ();
    type Error = ServiceError<C::Error, Codec>;
    type InitError = C::InitError;
    type Service = FramedServiceImpl<St, C::Service, T, Io, Codec>;
    type Future = LocalBoxFuture<'static, Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: Cfg) -> Self::Future {
        let handler = self.handler.clone();
        let disconnect = self.disconnect.clone();

        // create connect service and then create service impl
        self.connect
            .new_service(())
            .map(move |result| {
                result.map(move |connect| FramedServiceImpl {
                    connect,
                    handler,
                    disconnect,
                    _t: PhantomData,
                })
            })
            .boxed_local()
    }
}

pub struct FramedServiceImpl<St, C, T, Io, Codec> {
    connect: C,
    handler: Rc<T>,
    disconnect: Option<Rc<dyn Fn(&mut St, bool)>>,
    _t: PhantomData<(St, Io, Codec)>,
}

impl<St, C, T, Io, Codec> Service for FramedServiceImpl<St, C, T, Io, Codec>
where
    St: Clone,
    Io: AsyncRead + AsyncWrite,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<St, Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Future: 'static,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    type Request = Io;
    type Response = ();
    type Error = ServiceError<C::Error, Codec>;
    type Future = FramedServiceImplResponse<St, Io, Codec, C, T>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(|e| e.into())
    }

    fn call(&mut self, req: Io) -> Self::Future {
        FramedServiceImplResponse {
            inner: FramedServiceImplResponseInner::Connect(
                self.connect.call(Connect::new(req)),
                self.handler.clone(),
                self.disconnect.clone(),
            ),
        }
    }
}

#[pin_project::pin_project]
pub struct FramedServiceImplResponse<St, Io, Codec, C, T>
where
    St: Clone,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<St, Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    #[pin]
    inner: FramedServiceImplResponseInner<St, Io, Codec, C, T>,
}

impl<St, Io, Codec, C, T> Future for FramedServiceImplResponse<St, Io, Codec, C, T>
where
    St: Clone,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<St, Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
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
enum FramedServiceImplResponseInner<St, Io, Codec, C, T>
where
    St: Clone,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<St, Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    Connect(#[pin] C::Future, Rc<T>, Option<Rc<dyn Fn(&mut St, bool)>>),
    Handler(
        #[pin] T::Future,
        Option<ConnectResult<Io, St, Codec>>,
        Option<Rc<dyn Fn(&mut St, bool)>>,
    ),
    Dispatcher(#[pin] FramedDispatcher<St, T::Service, Io, Codec>),
}

impl<St, Io, Codec, C, T> FramedServiceImplResponseInner<St, Io, Codec, C, T>
where
    St: Clone,
    C: Service<Request = Connect<Io>, Response = ConnectResult<Io, St, Codec>>,
    C::Error: 'static,
    T: ServiceFactory<
        Config = St,
        Request = RequestItem<St, Codec>,
        Response = ResponseItem<Codec>,
        Error = C::Error,
        InitError = C::Error,
    >,
    <T::Service as Service>::Future: 'static,
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
{
    #[project]
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Either<
        FramedServiceImplResponseInner<St, Io, Codec, C, T>,
        Poll<Result<(), ServiceError<C::Error, Codec>>>,
    > {
        #[project]
        match self.project() {
            FramedServiceImplResponseInner::Connect(fut, handler, disconnect) => {
                match fut.poll(cx) {
                    Poll::Ready(Ok(res)) => {
                        Either::Left(FramedServiceImplResponseInner::Handler(
                            handler.new_service(res.state.clone()),
                            Some(res),
                            disconnect.take(),
                        ))
                    }
                    Poll::Pending => Either::Right(Poll::Pending),
                    Poll::Ready(Err(e)) => Either::Right(Poll::Ready(Err(e.into()))),
                }
            }
            FramedServiceImplResponseInner::Handler(fut, res, disconnect) => match fut.poll(cx)
            {
                Poll::Ready(Ok(handler)) => {
                    let res = res.take().unwrap();
                    Either::Left(FramedServiceImplResponseInner::Dispatcher(
                        FramedDispatcher::new(
                            res.framed,
                            res.state,
                            handler,
                            res.rx,
                            res.sink,
                            disconnect.take(),
                        ),
                    ))
                }
                Poll::Pending => Either::Right(Poll::Pending),
                Poll::Ready(Err(e)) => Either::Right(Poll::Ready(Err(e.into()))),
            },
            FramedServiceImplResponseInner::Dispatcher(ref mut fut) => {
                Either::Right(fut.poll(cx))
            }
        }
    }
}
