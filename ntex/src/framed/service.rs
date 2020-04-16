use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::{ready, Stream};

use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder};
use crate::service::{IntoService, IntoServiceFactory, Service, ServiceFactory};
use crate::util::framed::Dispatcher;

use super::handshake::{Handshake, HandshakeResult};
use super::ServiceError;

type RequestItem<U> = <U as Decoder>::Item;
type ResponseItem<U> = Option<<U as Encoder>::Item>;

/// Service builder - structure that follows the builder pattern
/// for building instances for framed services.
pub struct Builder<St, C, Io, Codec, Out> {
    connect: C,
    disconnect_timeout: usize,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, Io, Codec, Out> Builder<St, C, Io, Codec, Out>
where
    C: Service<
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Item: 'static,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    /// Construct framed handler service with specified connect service
    pub fn new<F>(connect: F) -> Builder<St, C, Io, Codec, Out>
    where
        F: IntoService<C>,
    {
        Builder {
            connect: connect.into_service(),
            disconnect_timeout: 3000,
            _t: PhantomData,
        }
    }

    /// Set connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, val: usize) -> Self {
        self.disconnect_timeout = val;
        self
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
            disconnect_timeout: self.disconnect_timeout,
            _t: PhantomData,
        }
    }
}

/// Service builder - structure that follows the builder pattern
/// for building instances for framed services.
pub struct FactoryBuilder<St, C, Io, Codec, Out> {
    connect: C,
    disconnect_timeout: usize,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, Io, Codec, Out> FactoryBuilder<St, C, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<
        Config = (),
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
    Codec: Decoder + Encoder,
    <Codec as Encoder>::Error: std::fmt::Debug,
    Out: Stream<Item = <Codec as Encoder>::Item> + Unpin,
{
    /// Construct framed handler service factory with specified connect service
    pub fn new<F>(connect: F) -> FactoryBuilder<St, C, Io, Codec, Out>
    where
        F: IntoServiceFactory<C>,
    {
        FactoryBuilder {
            connect: connect.into_factory(),
            disconnect_timeout: 3000,
            _t: PhantomData,
        }
    }

    /// Set connection disconnect timeout in milliseconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, val: usize) -> Self {
        self.disconnect_timeout = val;
        self
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
            disconnect_timeout: self.disconnect_timeout,
            _t: PhantomData,
        }
    }
}

pub struct FramedService<St, C, T, Io, Codec, Out, Cfg> {
    connect: C,
    handler: Rc<T>,
    disconnect_timeout: usize,
    _t: PhantomData<(St, Io, Codec, Out, Cfg)>,
}

impl<St, C, T, Io, Codec, Out, Cfg> ServiceFactory
    for FramedService<St, C, T, Io, Codec, Out, Cfg>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<
        Config = (),
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
    <C::Service as Service>::Future: 'static,
    T: ServiceFactory<
            Config = St,
            Request = RequestItem<Codec>,
            Response = ResponseItem<Codec>,
            Error = C::Error,
            InitError = C::Error,
        > + 'static,
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
            disconnect_timeout: self.disconnect_timeout,
        }
    }
}

#[pin_project::pin_project]
pub struct FramedServiceResponse<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<
        Config = (),
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
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
    disconnect_timeout: usize,
}

impl<St, C, T, Io, Codec, Out> Future for FramedServiceResponse<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: ServiceFactory<
        Config = (),
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
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
            disconnect_timeout: *this.disconnect_timeout,
            _t: PhantomData,
        }))
    }
}

pub struct FramedServiceImpl<St, C, T, Io, Codec, Out> {
    connect: C,
    handler: Rc<T>,
    disconnect_timeout: usize,
    _t: PhantomData<(St, Io, Codec, Out)>,
}

impl<St, C, T, Io, Codec, Out> Service for FramedServiceImpl<St, C, T, Io, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    C: Service<
        Request = Handshake<Io, Codec>,
        Response = HandshakeResult<Io, St, Codec, Out>,
    >,
    C::Error: fmt::Debug,
    C::Future: 'static,
    T: ServiceFactory<
            Config = St,
            Request = RequestItem<Codec>,
            Response = ResponseItem<Codec>,
            Error = C::Error,
            InitError = C::Error,
        > + 'static,
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
    type Future = Pin<Box<dyn Future<Output = Result<(), Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.connect.poll_ready(cx).map_err(|e| e.into())
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.connect.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: Io) -> Self::Future {
        log::trace!("Start connection handshake");

        let handler = self.handler.clone();
        let timeout = self.disconnect_timeout;
        let handshake = self.connect.call(Handshake::new(req));

        Box::pin(async move {
            let result = handshake.await.map_err(|e| {
                log::trace!("Connection handshake failed: {:?}", e);
                e
            })?;
            log::trace!("Connection handshake succeeded");

            let handler = handler.new_service(result.state).await?;
            log::trace!("Connection handler is created, starting dispatcher");

            Dispatcher::with(result.framed, result.out, handler)
                .disconnect_timeout(timeout as u64)
                .await
        })
    }
}
