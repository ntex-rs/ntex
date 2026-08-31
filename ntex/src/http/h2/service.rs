use std::{cell::RefCell, error::Error as StdError, future::poll_fn, io, marker, mem, rc::Rc};

use ntex_h2::{self as h2, frame::StreamId, server};

use crate::error::Error;
use crate::http::body::{BodySize, MessageBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, HttpPipeline, Method, Request, Response, StatusCode, Uri, Version};
use crate::io::{Filter, Io, IoBoxed, IoRef, types};
use crate::service::pipeline::{Pipeline, PipelineBinding, PipelineState};
use crate::service::state::RequestState;
use crate::service::{Ctx, Service, ServiceFactory};
use crate::service::{IntoService, IntoServiceFactory, cfg::SharedCfg};
use crate::util::{Bytes, BytesMut, HashMap, dyn_rc_err};

use super::{DefaultControlService, payload::Payload, payload::PayloadSender};

/// `ServiceFactory` implementation for HTTP2 transport
#[derive(derive_more::Debug)]
#[debug("H2Service")]
pub struct H2Service<St, Rst: RequestState<Res = Io<F>>, F, B, Err> {
    rst: Rst,
    sf: HttpPipeline<Rst::State, B, Err>,
    ctl: PipelineState<St, h2::Control<H2Error>, h2::ControlAck, Rc<dyn StdError>>,
    config: DispatcherConfig,
}

impl<St, Rst, F, B, Err> H2Service<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn new<Sf>(
        rst: Rst,
        sf: impl IntoServiceFactory<Sf, Rst::State, Request, SharedCfg>,
    ) -> Self
    where
        Sf: ServiceFactory<Rst::State, Request, SharedCfg, Error = Err> + 'static,
        Sf::Res: Into<Response<B>>,
        Sf::InitError: Into<Box<dyn StdError>>,
    {
        H2Service {
            rst,
            sf: HttpPipeline::new(sf.into_factory().map(Into::into).map_init_err(Into::into)),
            ctl: PipelineState::new(DefaultControlService),
            config: DispatcherConfig::default(),
        }
    }
}

impl<St, Rst, F, B, Err> H2Service<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: 'static,
{
    #[must_use]
    /// Provide http/2 control service
    pub fn control<Sf>(
        self,
        ctl: impl IntoService<Sf, St, h2::Control<H2Error>>,
    ) -> H2Service<St, Rst, F, B, Err>
    where
        Sf: Service<St, h2::Control<H2Error>, Res = h2::ControlAck> + 'static,
        Sf::Error: StdError + 'static,
    {
        H2Service {
            sf: self.sf,
            rst: self.rst,
            ctl: PipelineState::new(ctl.into_service().map_err(dyn_rc_err)),
            config: self.config,
        }
    }
}

impl<St, Rst, F, B, Err> Service<St, Rst::Req> for H2Service<St, Rst, F, B, Err>
where
    St: Clone + 'static,
    Rst: RequestState<Res = Io<F>>,
    F: Filter,
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.ctl.ready(ctx.st()).await.map_err(|e| {
            log::error!("Service readiness error: {e:?}");
            DispatchError::Control
        })
    }

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }

        self.ctl.shutdown(ctx.st()).await;
    }

    async fn call(&self, io: Rst::Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (io, st) = self.rst.map(io);

        let cfg = io.shared();
        let svc = self.sf.create(&cfg, st).await.map_err(|e| {
            log::error!("Cannot construct handler service: {e:?}");
            DispatchError::Control
        })?;

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let inflight = self.config.insert_io(&ioref);
        log::trace!(
            "{}: New http2 connection {id}, peer address {:?}, inflight: {inflight}",
            io.tag(),
            io.query::<types::PeerAddr>().get()
        );

        let result = handle(id, io.into(), svc, self.ctl.bind_state(ctx.st().clone())).await;

        let inflight = self.config.remove_io(&ioref);
        if inflight == 0 && self.config.is_shutdown() {
            self.config.notify_shutdown();
        }
        result
    }
}

pub(in crate::http) async fn handle<B, Err>(
    id: usize,
    io: IoBoxed,
    svc: Pipeline<Request, Response<B>, Err>,
    control: PipelineBinding<h2::Control<H2Error>, h2::ControlAck, Rc<dyn StdError>>,
) -> Result<(), DispatchError>
where
    B: MessageBody,
    Err: ResponseError + 'static,
{
    let ioref = io.get_ref();

    let _ = server::handle_one(
        io,
        Pipeline::with((), PublishService::new(id, ioref, svc)),
        control,
    )
    .await;

    Ok(())
}

struct PublishService<B, Err> {
    id: usize,
    io: IoRef,
    svc: Pipeline<Request, Response<B>, Err>,
    streams: RefCell<HashMap<StreamId, PayloadSender>>,
    _t: marker::PhantomData<B>,
}

impl<B, Err> PublishService<B, Err>
where
    B: MessageBody,
    Err: ResponseError,
{
    fn new(id: usize, io: IoRef, svc: Pipeline<Request, Response<B>, Err>) -> Self {
        Self {
            id,
            io,
            svc,
            streams: RefCell::new(HashMap::default()),
            _t: marker::PhantomData,
        }
    }
}

impl<B, Err> Service<(), h2::Message> for PublishService<B, Err>
where
    B: MessageBody,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = H2Error;

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.svc.shutdown().await;
    }

    async fn call(&self, msg: h2::Message, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        let h2::Message { stream, kind } = msg;
        let (io, pseudo, headers, eof, payload) = match kind {
            h2::MessageKind::Headers {
                pseudo,
                headers,
                eof,
            } => {
                let pl = if eof {
                    None
                } else {
                    #[cfg(feature = "trace")]
                    log::debug!(
                        "{}: Creating local payload stream for {:?}",
                        self.io.tag(),
                        stream.id()
                    );
                    let (sender, payload) = Payload::create(stream.empty_capacity());
                    self.streams.borrow_mut().insert(stream.id(), sender);
                    Some(payload)
                };
                (self.io.clone(), pseudo, headers, eof, pl)
            }
            h2::MessageKind::Data(data, cap) => {
                #[cfg(feature = "trace")]
                log::debug!(
                    "{}: Got data chunk for {:?}: {:?}",
                    self.io.tag(),
                    stream.id(),
                    data.len()
                );
                if let Some(sender) = self.streams.borrow_mut().get_mut(&stream.id()) {
                    sender.feed_data(data, cap);
                } else {
                    log::error!(
                        "{}: Payload stream does not exists for {:?}",
                        self.io.tag(),
                        stream.id()
                    );
                }
                return Ok(());
            }
            h2::MessageKind::Eof(item) => {
                log::debug!(
                    "{}: Got payload eof for {:?}: {item:?}",
                    self.io.tag(),
                    stream.id()
                );
                if let Some(sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    match item {
                        h2::StreamEof::Data(data) => {
                            sender.feed_eof(data);
                        }
                        h2::StreamEof::Trailers(_) => {
                            sender.feed_eof(Bytes::new());
                        }
                        h2::StreamEof::Error(err) => {
                            sender.set_error(err.into_error().into());
                        }
                    }
                }
                return Ok(());
            }
            h2::MessageKind::Disconnect(err) => {
                log::debug!("{}: Connection is disconnected {err:?}", self.io.tag());
                if let Some(sender) = self.streams.borrow_mut().remove(&stream.id()) {
                    sender.set_error(io::Error::new(io::ErrorKind::UnexpectedEof, err).into());
                }
                return Ok(());
            }
        };

        log::trace!(
            "{}: {:?} got request (eof: {eof}): {pseudo:#?}\nheaders: {headers:#?}",
            self.io.tag(),
            stream.id()
        );
        let mut req = if let Some(pl) = payload {
            Request::with_payload(crate::http::Payload::H2(pl))
        } else {
            Request::new()
        };

        let path = pseudo.path.ok_or(H2Error::MissingPseudo("Path"))?;
        let method = pseudo.method.ok_or(H2Error::MissingPseudo("Method"))?;

        let head = req.head_mut();
        head.uri = if let Some(ref authority) = pseudo.authority {
            let scheme = pseudo.scheme.ok_or(H2Error::MissingPseudo("Scheme"))?;
            Uri::try_from(format!("{scheme}://{authority}{path}"))?
        } else {
            Uri::try_from(path.as_str())?
        };
        let is_head_req = method == Method::HEAD;
        head.version = Version::HTTP_2;
        head.method = method;
        head.headers = headers;
        head.io = CurrentIo::Ref(io);
        head.id = self.id;

        let (mut res, mut body) = match self.svc.call(req).await {
            Ok(res) => res.into_parts(),
            Err(err) => {
                let (res, body) = Response::from(&err).into_parts();
                (res, body.into_body())
            }
        };

        let head = res.head_mut();
        let mut size = body.size();
        prepare_response(head, &mut size);

        #[cfg(feature = "trace")]
        log::debug!(
            "{}: Received service response: {head:?} payload: {size:?}",
            self.io.tag()
        );

        let hdrs = mem::replace(&mut head.headers, HeaderMap::new());
        if size.is_eof() || is_head_req {
            stream
                .send_response(head.status, hdrs, true)
                .map_err(Error::into_error)?;
        } else {
            stream
                .send_response(head.status, hdrs, false)
                .map_err(Error::into_error)?;

            loop {
                match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                    None => {
                        #[cfg(feature = "trace")]
                        log::debug!(
                            "{}: {:?} closing payload stream",
                            self.io.tag(),
                            stream.id()
                        );
                        stream
                            .send_payload(Bytes::new(), true)
                            .await
                            .map_err(Error::into_error)?;
                        break;
                    }
                    Some(Ok(chunk)) => {
                        #[cfg(feature = "trace")]
                        log::debug!(
                            "{}: {:?} sending data chunk {:?} bytes",
                            self.io.tag(),
                            stream.id(),
                            chunk.len()
                        );
                        if !chunk.is_empty() {
                            stream
                                .send_payload(chunk, false)
                                .await
                                .map_err(Error::into_error)?;
                        }
                    }
                    Some(Err(e)) => {
                        #[cfg(feature = "trace")]
                        log::error!("{}: Response payload stream error: {e:?}", self.io.tag());
                        return Err(H2Error::Stream(e));
                    }
                }
            }
        }
        Ok(())
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_CONTENT_LENGTH: HeaderValue = HeaderValue::from_static("0");
#[allow(clippy::declare_interior_mutable_const)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
#[allow(clippy::declare_interior_mutable_const)]
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

fn prepare_response(head: &mut ResponseHead, size: &mut BodySize) {
    // Content length
    match head.status {
        StatusCode::NO_CONTENT | StatusCode::CONTINUE | StatusCode::PROCESSING => {
            *size = BodySize::None;
        }
        StatusCode::SWITCHING_PROTOCOLS => {
            *size = BodySize::Stream;
        }
        _ => (),
    }
    match size {
        BodySize::None | BodySize::Stream => head.headers.remove(header::CONTENT_LENGTH),
        BodySize::Empty => head
            .headers
            .insert(header::CONTENT_LENGTH, ZERO_CONTENT_LENGTH),
        BodySize::Sized(len) => {
            let mut buf = BytesMut::new();
            crate::http::h1::encoder::convert_usize(*len, &mut buf, false);
            head.headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_shared(buf.freeze()).unwrap(),
            );
        }
    }

    // http2 specific1
    head.headers.remove(header::CONNECTION);
    head.headers.remove(header::TRANSFER_ENCODING);
    head.headers.remove(header::UPGRADE);

    // omit HTTP/1.x only headers according to:
    // https://datatracker.ietf.org/doc/html/rfc7540#section-8.1.2.2
    head.headers.remove(KEEP_ALIVE);
    head.headers.remove(PROXY_CONNECTION);

    // set date header
    if !head.headers.contains_key(header::DATE) {
        let mut bytes = BytesMut::with_capacity(29);
        DateService::set_date(|date| bytes.extend_from_slice(date));
        head.headers.insert(header::DATE, unsafe {
            HeaderValue::from_shared_unchecked(bytes.freeze())
        });
    }
}
