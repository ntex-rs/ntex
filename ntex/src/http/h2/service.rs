use std::{cell::Cell, cell::RefCell, future::poll_fn, io, mem, pin::Pin, rc::Rc};

use ntex_h2::{self as h2, control::ExpectResult, frame::StreamId, server};
use urly::Url;

use crate::error::{Error, IntoFailure};
use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::config::{DispatcherConfig, HttpServiceConfig};
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::helpers::{TargetPolicy, process_request_target};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Version};
use crate::io::{Filter, Io, IoBoxed, IoRef, types};
use crate::service::cfg::Configuration;
use crate::service::pipeline::{Pipeline, PipelineBinding, PipelineFactory};
use crate::service::{BindCtx, Ctx, IntoServiceFactory, RequestState, Service, ServiceFactory};
use crate::util::{ByteString, Bytes, HashMap};

use super::{payload::Payload, payload::PayloadSender};

/// An HTTP/2 transport service.
///
/// Construct this service with [`HttpService::h2`](crate::http::HttpService::h2).
/// Request handling is delegated to the configured application service, while
/// control events get their default action unless a control service is
/// provided with [`control`](Self::control).
///
/// The application and control factories are instantiated for every accepted
/// connection; see [factory scope](crate::http::HttpService#factory-scope).
#[derive(derive_more::Debug)]
#[debug("H2Service")]
pub struct H2Service<F, Req: RequestState<Io<F>>, Err> {
    sf: crate::http::HttpPipeline<Req::State, Err>,
    /// Without a control service the default action is applied to every event.
    ctl: Option<crate::http::Ctl2Pipeline<Req::State>>,
    config: DispatcherConfig,
}

impl<F, Req, Err> H2Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    /// Create new `H2Service` instance.
    pub(crate) fn new<Sf>(sf: impl IntoServiceFactory<Sf, Req::State, Request>) -> Self
    where
        Sf: ServiceFactory<Req::State, Request, Error = Err> + 'static,
        Sf::Res: Into<Response>,
        Sf::InitError: IntoFailure,
    {
        H2Service {
            sf: PipelineFactory::new(
                sf.into_factory()
                    .map(Into::into)
                    .map_init_err(|e| DispatchError::Control(e.fail())),
            ),
            ctl: None,
            config: DispatcherConfig::default(),
        }
    }
}

impl<F, Req, Err> H2Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    #[must_use]
    /// Provides the HTTP/2 control service.
    ///
    /// Unless this method is called, the default action is applied to each
    /// control event without a service call.
    pub fn control<I, Sf>(self, ctl: I) -> Self
    where
        I: IntoServiceFactory<Sf, Req::State, h2::Control<Error<H2Error>>>,
        Sf: ServiceFactory<Req::State, h2::Control<Error<H2Error>>, Res = h2::ControlAck> + 'static,
        Sf::Error: IntoFailure,
        Sf::InitError: IntoFailure,
    {
        H2Service {
            sf: self.sf,
            ctl: Some(PipelineFactory::new(
                ctl.into_factory()
                    .map_err(|e| DispatchError::Service(e.fail()))
                    .map_init_err(|e| DispatchError::Service(e.fail())),
            )),
            config: self.config,
        }
    }
}

impl<St, F, Req, Err> Service<St, Req> for H2Service<F, Req, Err>
where
    F: Filter,
    Req: RequestState<Io<F>>,
    Req::State: Clone,
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = DispatchError;

    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (st, io) = req.unpack();

        let svc = self.sf.create(st.clone()).await?;
        let ctl = match &self.ctl {
            Some(ctl) => Some(ctl.create(st).await?),
            None => None,
        };

        let id = self.config.next_id();
        let ioref = io.get_ref();
        let (_guard, inflight) = self.config.insert_io(&ioref);
        log::trace!(
            "{}: New http2 connection {id}, peer address {:?}, inflight: {inflight}",
            io.tag(),
            io.query::<types::PeerAddr>().get()
        );

        handle(id, io.into(), svc, ctl).await
    }

    #[inline]
    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    #[inline]
    async fn shutdown(&self, _: Ctx<'_, Self, St>) {
        // check inflight connections
        let inflight = self.config.shutdown();
        if inflight != 0 {
            log::trace!("Shutting down service, in-flight connections: {inflight}");

            self.config.wait_shutdown().await;
            log::trace!("Shutting down is complected");
        }
    }
}

pub(in crate::http) async fn handle<Err>(
    id: usize,
    io: IoBoxed,
    svc: Pipeline<Request, Response, Err>,
    control: Option<Pipeline<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>>,
) -> Result<(), DispatchError>
where
    Err: ResponseError + 'static,
{
    let ioref = io.get_ref();
    let control = Pipeline::new((), ControlService { inner: control });

    let _ = server::handle_one(
        io,
        Pipeline::new((), PublishService::new(id, ioref, svc, control.bind())),
        control,
    )
    .await;

    Ok(())
}

/// Sets `GOAWAY` reason codes for connection level request errors.
///
/// Without a user control service the default action is applied to every event.
struct ControlService {
    inner: Option<Pipeline<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>>,
}

impl Service<(), h2::Control<Error<H2Error>>> for ControlService {
    type Res = h2::ControlAck;
    type Error = DispatchError;

    fn bind(&self, ctx: BindCtx<'_>) {
        if let Some(inner) = &self.inner {
            inner.bind_to(&ctx);
        }
    }

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        if let Some(inner) = &self.inner {
            inner.ready().await
        } else {
            Ok(())
        }
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        if let Some(inner) = &self.inner {
            inner.shutdown().await;
        }
    }

    async fn call(
        &self,
        msg: h2::Control<Error<H2Error>>,
        _: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        let msg = match msg {
            h2::Control::Disconnect(h2::control::Reason::Error(err))
                if matches!(**err.get_ref(), H2Error::EmptyDataFrames) =>
            {
                h2::Control::Disconnect(h2::control::Reason::Error(
                    err.reason(h2::frame::Reason::ENHANCE_YOUR_CALM),
                ))
            }
            msg => msg,
        };
        if let Some(inner) = &self.inner {
            inner.call(msg).await
        } else {
            log::trace!("HTTP/2 Control message: {msg:?}");
            Ok(msg.ack())
        }
    }
}

struct PublishService<Err> {
    id: usize,
    target_policy: TargetPolicy,
    /// `None` if the h2 decoder limit already enforces `HttpServiceConfig::max_headers`
    max_headers: Option<usize>,
    io: IoRef,
    svc: Pipeline<Request, Response, Err>,
    control: PipelineBinding<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>,
    streams: Rc<RefCell<HashMap<StreamId, StreamPayload>>>,
    /// Consecutive empty non-final `DATA` frames
    empty_data: Cell<u8>,
}

/// Maximum number of consecutive empty non-final `DATA` frames,
/// such frames are not flow controlled
const MAX_EMPTY_DATA_FRAMES: u8 = 10;

/// Request payload of a stream.
struct StreamPayload {
    sender: PayloadSender,
    /// The response is complete
    complete: bool,
}

impl<Err> PublishService<Err>
where
    Err: ResponseError,
{
    fn new(
        id: usize,
        io: IoRef,
        svc: Pipeline<Request, Response, Err>,
        control: PipelineBinding<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>,
    ) -> Self {
        let cfg = io.cfg().ctx().get::<HttpServiceConfig>();
        let max_headers = usize::from(cfg.max_headers);
        // the h2 decoder refuses header blocks over its own limit, it counts
        // fields the same way, so an equal or larger limit is always met
        let h2_max_headers = cfg.get_shared::<h2::ServiceConfig>().max_headers();
        Self {
            id,
            target_policy: cfg.target_policy(),
            max_headers: (max_headers < h2_max_headers).then_some(max_headers),
            io,
            svc,
            control,
            streams: Rc::new(RefCell::new(HashMap::default())),
            empty_data: Cell::new(0),
        }
    }
}

impl<Err> PublishService<Err> {
    /// Answers a rejected request with an empty response, the connection stays open.
    fn reject(&self, stream: &h2::StreamRef, status: StatusCode, eof: bool) {
        self.streams.borrow_mut().remove(&stream.id());

        let mut res = Response::new(status).drop_body();
        let head = res.head_mut();
        prepare_response(head, &mut BodySize::Empty);
        let _ = stream.send_response(status, &head.headers, true);
        if !eof {
            // the request body is not needed
            stream.reset(h2::frame::Reason::NO_ERROR);
        }
    }

    /// Starts a request: registers its payload, validates the request head and
    /// builds the request.
    ///
    /// Runs before the publish future is created, so the pseudo headers and
    /// the request target are not kept in it. Only `Expect: 100-continue`
    /// waits for the control service.
    fn request_head(
        &self,
        stream: h2::StreamRef,
        pseudo: h2::frame::PseudoHeaders,
        headers: HeaderMap,
        eof: bool,
    ) -> Start<'_> {
        let payload = if eof {
            None
        } else {
            #[cfg(feature = "trace")]
            log::debug!(
                "{}: Creating local payload stream for {:?}",
                self.io.tag(),
                stream.id()
            );
            let (sender, payload) = Payload::create(stream.empty_capacity());
            self.streams.borrow_mut().insert(
                stream.id(),
                StreamPayload {
                    sender,
                    complete: false,
                },
            );
            Some(payload)
        };

        // repeated header names count separately, like HTTP/1
        if let Some(max) = self.max_headers
            && headers.iter().nth(max).is_some()
        {
            log::debug!("{}: Too many headers on {:?}", self.io.tag(), stream.id());
            self.reject(&stream, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE, eof);
            return Start::Done(Ok(()));
        }

        // a malformed request is a stream error, the connection stays open,
        // see RFC 9113 section 8.1.1
        let Some(target) = request_uri(&pseudo, self.target_policy) else {
            log::debug!(
                "{}: Malformed request on {:?}: {pseudo:?}",
                self.io.tag(),
                stream.id()
            );
            self.reject(&stream, StatusCode::BAD_REQUEST, eof);
            return Start::Done(Ok(()));
        };

        // the client waits for `100 Continue` before sending the request body,
        // see RFC 9110 section 10.1.1
        if !eof && expect_continue(&headers) {
            return Start::Expect(Box::pin(
                self.expect_continue(stream, target, pseudo, headers, payload),
            ));
        }

        log::trace!(
            "{}: {:?} got request (eof: {eof}): {pseudo:#?}\nheaders: {headers:#?}",
            self.io.tag(),
            stream.id()
        );
        Start::Request(self.request(stream, target, headers, payload))
    }

    /// Answers `Expect: 100-continue` with the control service, then builds
    /// the request.
    async fn expect_continue(
        &self,
        stream: h2::StreamRef,
        target: Target,
        pseudo: h2::frame::PseudoHeaders,
        headers: HeaderMap,
        payload: Option<Payload>,
    ) -> Option<NewRequest> {
        let msg = h2::Control::expect(stream.clone(), pseudo, headers);
        let (pseudo, headers) = match self
            .control
            .call(msg)
            .await
            .map(h2::ControlAck::into_expect)
        {
            Ok(Some(ExpectResult::Continue(expect))) => {
                if stream
                    .send_informational(StatusCode::CONTINUE, HeaderMap::new())
                    .is_err()
                {
                    // the stream is closed
                    self.streams.borrow_mut().remove(&stream.id());
                    return None;
                }
                let (_, pseudo, headers) = expect.into_parts();
                (pseudo, headers)
            }
            Ok(Some(ExpectResult::Failed(_, status, headers))) => {
                self.streams.borrow_mut().remove(&stream.id());

                let mut res = Response::new(status).drop_body();
                let head = res.head_mut();
                h2::release_header_map(mem::replace(&mut head.headers, headers));
                prepare_response(head, &mut BodySize::Empty);
                let _ = stream.send_response(status, &head.headers, true);

                // the request body is not needed
                stream.reset(h2::frame::Reason::NO_ERROR);
                return None;
            }
            result => {
                log::error!(
                    "{}: Control service failed to handle expect for {:?}: {:?}",
                    self.io.tag(),
                    stream.id(),
                    result.map(|_| ())
                );
                self.streams.borrow_mut().remove(&stream.id());
                stream.reset(h2::frame::Reason::INTERNAL_ERROR);
                return None;
            }
        };

        log::trace!(
            "{}: {:?} got request (eof: false): {pseudo:#?}\nheaders: {headers:#?}",
            self.io.tag(),
            stream.id()
        );
        Some(self.request(stream, target, headers, payload))
    }

    /// Builds the request for the application service.
    fn request(
        &self,
        stream: h2::StreamRef,
        target: Target,
        headers: HeaderMap,
        payload: Option<Payload>,
    ) -> NewRequest {
        let mut req = if let Some(pl) = payload {
            Request::with_payload(crate::http::Payload::H2(pl))
        } else {
            Request::new()
        };

        let is_head_req = target.method == Method::HEAD;
        let head = req.head_mut();
        head.set_stream(stream.clone());
        head.uri = target.uri;
        head.scheme = target.scheme;
        head.authority = target.authority;
        head.version = Version::HTTP_2;
        head.method = target.method;
        // the pooled head map is empty, the h2 decoder reuses its allocation
        h2::release_header_map(mem::replace(&mut head.headers, headers));
        head.io = CurrentIo::Ref(self.io.clone());
        head.id = self.id;

        NewRequest {
            stream,
            req,
            is_head_req,
        }
    }

    /// Feeds a `DATA` frame to the request payload.
    fn data(
        &self,
        stream: &h2::StreamRef,
        data: Bytes,
        cap: h2::Capacity,
    ) -> Result<(), Error<H2Error>> {
        #[cfg(feature = "trace")]
        log::debug!(
            "{}: Got data chunk for {:?}: {:?}",
            self.io.tag(),
            stream.id(),
            data.len()
        );
        // the limit is sticky, frames buffered after it are not delivered
        let count = if data.is_empty() {
            self.empty_data.get().saturating_add(1)
        } else if self.empty_data.get() >= MAX_EMPTY_DATA_FRAMES {
            MAX_EMPTY_DATA_FRAMES
        } else {
            0
        };
        self.empty_data.set(count);
        if count >= MAX_EMPTY_DATA_FRAMES {
            log::debug!(
                "{}: Too many consecutive empty DATA frames, closing connection",
                self.io.tag()
            );
            return Err(H2Error::EmptyDataFrames.into());
        }
        let mut streams = self.streams.borrow_mut();
        if let Some(pl) = streams.get(&stream.id()) {
            if pl.complete && pl.sender.is_dropped() {
                // the response is complete, the request body is dropped unread
                streams.remove(&stream.id());
                drop(streams);
                stream.reset(h2::frame::Reason::NO_ERROR);
            } else {
                pl.sender.feed_data(data, cap);
            }
        } else {
            log::error!(
                "{}: Payload stream does not exists for {:?}",
                self.io.tag(),
                stream.id()
            );
        }
        Ok(())
    }

    /// Completes the request payload.
    fn eof(&self, stream: &h2::StreamRef, item: h2::StreamEof) -> Result<(), Error<H2Error>> {
        log::debug!(
            "{}: Got payload eof for {:?}: {item:?}",
            self.io.tag(),
            stream.id()
        );
        if self.empty_data.get() >= MAX_EMPTY_DATA_FRAMES {
            return Err(H2Error::EmptyDataFrames.into());
        }
        if matches!(item, h2::StreamEof::Data(ref data, _) if !data.is_empty()) {
            self.empty_data.set(0);
        }
        if let Some(StreamPayload { sender, .. }) = self.streams.borrow_mut().remove(&stream.id()) {
            match item {
                h2::StreamEof::Data(data, cap) => {
                    sender.feed_eof(data, Some(cap));
                }
                h2::StreamEof::Trailers(hdrs) => {
                    sender.feed_trailers(hdrs);
                }
                h2::StreamEof::Error(err) => {
                    sender.set_error(err.into_error().into());
                }
            }
        }
        Ok(())
    }
}

/// The synchronous part of handling an [`h2::Message`].
enum Start<'a> {
    /// The message is handled.
    Done(Result<(), Error<H2Error>>),
    /// The request is ready for the application service.
    Request(NewRequest),
    /// The client waits for `100 Continue`, `None` if the stream is answered.
    Expect(Pin<Box<dyn Future<Output = Option<NewRequest>> + 'a>>),
}

/// A request ready for the application service.
struct NewRequest {
    stream: h2::StreamRef,
    req: Request,
    is_head_req: bool,
}

impl<Err> Service<(), h2::Message> for PublishService<Err>
where
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = Error<H2Error>;

    // Keep payload delivery independent of application capacity.
    // New requests check readiness through `self.svc.call`.
    fn bind(&self, ctx: BindCtx<'_>) {
        self.svc.bind_to(&ctx);
        self.control.bind_to(&ctx);
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.svc.shutdown().await;
    }

    // A plain fn keeps the message out of the publish future,
    // an `async fn` would store it next to the service call.
    fn call(
        &self,
        msg: h2::Message,
        _: Ctx<'_, Self, ()>,
    ) -> impl Future<Output = Result<Self::Res, Self::Error>> {
        let h2::Message { stream, kind } = msg;
        let start = match kind {
            h2::MessageKind::Headers {
                pseudo,
                headers,
                eof,
            } => self.request_head(stream, pseudo, headers, eof),
            h2::MessageKind::Data(data, cap) => Start::Done(self.data(&stream, data, cap)),
            h2::MessageKind::Eof(item) => Start::Done(self.eof(&stream, item)),
            h2::MessageKind::Disconnect(err) => {
                log::debug!("{}: Connection is disconnected {err:?}", self.io.tag());
                if let Some(pl) = self.streams.borrow_mut().remove(&stream.id()) {
                    pl.sender
                        .set_error(io::Error::new(io::ErrorKind::UnexpectedEof, err).into());
                }
                Start::Done(Ok(()))
            }
        };

        async move {
            let NewRequest {
                stream,
                req,
                is_head_req,
            } = match start {
                Start::Done(result) => return result,
                Start::Request(req) => req,
                Start::Expect(fut) => match fut.await {
                    Some(req) => req,
                    None => return Ok(()),
                },
            };

            let result = self.svc.call(req).await;
            let (mut res, mut body) = Response::from(result).into_parts();

            // an interim response cannot complete the request and `101` is not
            // supported in HTTP/2, see RFC 9113 section 8.1 and 8.6
            let status = res.status();
            if status.is_informational() {
                log::error!(
                    "{}: Informational response {status} is not supported, sending 500",
                    self.io.tag()
                );
                res = Response::new(StatusCode::INTERNAL_SERVER_ERROR).drop_body();
                body = ResponseBody::Other(Body::Empty);
            }

            let head = res.head_mut();
            let mut size = body.size();
            prepare_response(head, &mut size);

            #[cfg(feature = "trace")]
            log::debug!(
                "{}: Received service response: {head:?} payload: {size:?}",
                self.io.tag()
            );

            // borrowed, the pooled response head keeps its header map capacity
            let hdrs = &head.headers;
            // `Err(Some(_))` is a body error, `Err(None)` is a closed stream
            let sent = async {
                if size.is_eof() || is_head_req {
                    stream
                        .send_response(head.status, hdrs, true)
                        .map_err(|_| None)?;
                    return Ok(());
                }
                stream
                    .send_response(head.status, hdrs, false)
                    .map_err(|_| None)?;

                // the last chunk of a sized body ends the stream, no empty eof frame
                let mut remaining = if let BodySize::Sized(len) = size {
                    Some(len)
                } else {
                    None
                };
                loop {
                    match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                        None => {
                            #[cfg(feature = "trace")]
                            log::debug!(
                                "{}: {:?} closing payload stream",
                                self.io.tag(),
                                stream.id()
                            );
                            return stream
                                .send_payload(Bytes::new(), true)
                                .await
                                .map_err(|_| None);
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
                                let eof = remaining.as_mut().is_some_and(|rem| {
                                    *rem = rem.saturating_sub(chunk.len() as u64);
                                    *rem == 0
                                });
                                stream.send_payload(chunk, eof).await.map_err(|_| None)?;
                                if eof {
                                    return Ok(());
                                }
                            }
                        }
                        Some(Err(e)) => return Err(Some(e)),
                    }
                }
            }
            .await;

            match sent {
                Ok(()) => (),
                Err(Some(e)) => {
                    // only the stream fails, the connection stays open
                    log::error!(
                        "{}: Response payload stream error for {:?}: {e:?}",
                        self.io.tag(),
                        stream.id()
                    );
                    stream.reset(h2::frame::Reason::INTERNAL_ERROR);
                    return Ok(());
                }
                Err(None) => {
                    // the stream is reset or the connection is closed
                    self.streams.borrow_mut().remove(&stream.id());
                    return Ok(());
                }
            }

            // the response is complete, an unread request body must not keep the stream
            let id = stream.id();
            let mut streams = self.streams.borrow_mut();
            if let Some(pl) = streams.get_mut(&id) {
                if pl.sender.is_dropped() {
                    streams.remove(&id);
                    drop(streams);
                    stream.reset(h2::frame::Reason::NO_ERROR);
                } else {
                    // the app still holds the request body, release the stream once it is dropped
                    pl.complete = true;
                    let streams = Rc::downgrade(&self.streams);
                    pl.sender.on_drop(move || {
                        if let Some(streams) = streams.upgrade()
                            && let Ok(mut streams) = streams.try_borrow_mut()
                        {
                            streams.remove(&id);
                        }
                        stream.reset(h2::frame::Reason::NO_ERROR);
                    });
                }
            }
            Ok(())
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_CONTENT_LENGTH: HeaderValue = HeaderValue::from_static("0");
#[allow(clippy::declare_interior_mutable_const)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
#[allow(clippy::declare_interior_mutable_const)]
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

/// Request target built from HTTP/2 pseudo-headers.
struct Target {
    method: Method,
    uri: Url,
    /// `:scheme`, `None` for `CONNECT`.
    scheme: Option<ByteString>,
    /// `:authority`, `None` for `CONNECT`, whose `uri` holds it.
    authority: Option<ByteString>,
}

/// Builds the request target from HTTP/2 pseudo-headers, validating and
/// normalizing it according to `policy`.
///
/// A `CONNECT` request gets its authority-form target as the URI. Other
/// requests get an origin-form URI from `:path`, while `:scheme` and
/// `:authority` are kept separately, see RFC 9113 section 8.3.1.
fn request_uri(pseudo: &h2::frame::PseudoHeaders, policy: TargetPolicy) -> Option<Target> {
    let method = pseudo.method.clone()?;
    // Mandatory pseudo-header presence and CONNECT combinations are checked by ntex-h2.
    if method == Method::CONNECT {
        let authority = pseudo.authority.as_ref()?;
        let mut uri = Url::from_target_parts(None, Some(authority), "").ok()?;
        process_request_target(policy, &method, Version::HTTP_2, &mut uri).ok()?;
        return Some(Target {
            method,
            uri,
            scheme: None,
            authority: None,
        });
    }

    let scheme = pseudo.scheme.as_ref()?;
    let path = pseudo.path.as_ref()?;
    // `parse_target_str` reads other targets as absolute- or authority-form,
    // `:path` must not supply an authority
    let mut uri = if path.starts_with('/') || path == "*" {
        Url::parse_target_str(path.clone(), path.find('?'))
    } else {
        Url::from_target_parts(None, None, path)
    }
    .ok()?;
    process_request_target(policy, &method, Version::HTTP_2, &mut uri).ok()?;

    let authority = pseudo.authority.as_ref();
    let normalize = policy == TargetPolicy::Normalize;
    let (scheme, authority) = match policy {
        TargetPolicy::Raw => (scheme.clone(), authority.cloned()),
        _ if check_scheme_authority(scheme, authority.map(ByteString::as_str), normalize)? => {
            (scheme.clone(), authority.cloned())
        }
        _ => {
            let mut base =
                Url::from_target_parts(Some(scheme), authority.map(ByteString::as_str), "/")
                    .ok()?;
            base.validate_target().ok()?;
            if normalize {
                base.normalize().ok()?;
            }
            (
                same_or_copy(scheme, base.scheme_str()?),
                authority
                    .zip(base.authority())
                    .map(|(orig, a)| same_or_copy(orig, a.as_str())),
            )
        }
    };
    Some(Target {
        method,
        uri,
        scheme: Some(scheme),
        authority,
    })
}

/// Checks `:scheme` and `:authority` like `Url::validate_target`, without building a url.
///
/// Returns `None` if the target is invalid, `Some(true)` if it is valid and, when
/// `normalize` is set, already normalized, and `Some(false)` if it needs the full check:
/// an encoded host must also be valid after decoding, and a non-canonical scheme or
/// authority must be normalized.
fn check_scheme_authority(scheme: &str, authority: Option<&str>, normalize: bool) -> Option<bool> {
    if authority.is_some_and(|a| a.contains('%')) {
        return Some(false);
    }
    let scheme = urly::Scheme::new(scheme).ok()?;
    let Some(authority) = authority else {
        return Some(!normalize || !has_uppercase(scheme.as_str()));
    };
    let a = urly::Authority::new(authority).ok()?;
    let host = a.host();
    if a.userinfo().is_some() || host.is_empty() {
        return None;
    }
    if !normalize {
        return Some(true);
    }
    // normalization lowercases the scheme and host, canonicalizes IPv6, drops an
    // empty or default port and strips leading zeros from the port, the authority
    // starts with the host as it has no userinfo
    let port = authority[host.len()..].strip_prefix(':');
    Some(
        !has_uppercase(scheme.as_str())
            && !has_uppercase(host)
            && !host.starts_with('[')
            && port.is_none_or(|p| {
                !p.is_empty()
                    && (p.len() == 1 || !p.starts_with('0'))
                    && p.parse::<u16>().ok() != scheme.default_port()
            }),
    )
}

fn has_uppercase(s: &str) -> bool {
    s.bytes().any(|b| b.is_ascii_uppercase())
}

/// Returns `orig` if `value` equals it, so it keeps sharing the decode buffer.
fn same_or_copy(orig: &ByteString, value: &str) -> ByteString {
    if orig.as_str() == value {
        orig.clone()
    } else {
        ByteString::from(value)
    }
}

/// Checks the case-insensitive `100-continue` expectation, see RFC 9110 section 10.1.1
fn expect_continue(headers: &HeaderMap) -> bool {
    headers.get_all(header::EXPECT).any(|value| {
        value
            .as_bytes()
            .split(|&b| b == b',')
            .any(|e| e.trim_ascii().eq_ignore_ascii_case(b"100-continue"))
    })
}

fn prepare_response(head: &mut ResponseHead, size: &mut BodySize) {
    // Content length
    // `204` and `304` responses never have a body, see RFC 9110 section 15.3.5 and 15.4.5
    if head.status == StatusCode::NO_CONTENT || head.status == StatusCode::NOT_MODIFIED {
        *size = BodySize::None;
    }

    // Find the headers to update in one pass, a lookup hashes the name even
    // if the header is absent
    let mut has_length = false;
    let mut has_date = false;
    let mut has_conn_specific = false;
    for name in head.headers.keys() {
        match *name {
            header::CONTENT_LENGTH => has_length = true,
            header::DATE => has_date = true,
            header::CONNECTION | header::TRANSFER_ENCODING | header::UPGRADE => {
                has_conn_specific = true;
            }
            _ => has_conn_specific |= *name == KEEP_ALIVE || *name == PROXY_CONNECTION,
        }
    }

    match size {
        BodySize::None | BodySize::Stream => {
            if has_length {
                head.headers.remove(header::CONTENT_LENGTH);
            }
        }
        BodySize::Empty => head
            .headers
            .insert(header::CONTENT_LENGTH, ZERO_CONTENT_LENGTH),
        BodySize::Sized(len) => {
            head.headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(*len));
        }
    }

    // omit connection-specific headers, see RFC 9113 section 8.2.2
    if has_conn_specific {
        head.headers.remove(header::CONNECTION);
        head.headers.remove(header::TRANSFER_ENCODING);
        head.headers.remove(header::UPGRADE);
        head.headers.remove(KEEP_ALIVE);
        head.headers.remove(PROXY_CONNECTION);
    }

    // set date header
    if !has_date {
        head.headers
            .insert(header::DATE, DateService::header_value());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_target(pseudo: &h2::frame::PseudoHeaders) -> bool {
        assert!(request_uri(pseudo, TargetPolicy::Raw).is_some());
        let valid = request_uri(pseudo, TargetPolicy::Validate).is_some();
        assert_eq!(
            request_uri(pseudo, TargetPolicy::Normalize).is_some(),
            valid
        );
        valid
    }

    /// STATEMENT: HTTP/2 targets keep `:path` as an origin-form uri with `:scheme` and
    /// `:authority` stored beside it, and target validation and normalization are opt-in.
    ///
    /// HOW:
    /// - Build targets from pseudo headers, including CONNECT, OPTIONS and double-slash paths.
    /// - Check the uri, scheme and authority with processing disabled, and that malformed
    ///   values are rejected and components are normalized when enabled.
    /// - Check that a `:path` in absolute- or authority-form does not supply the uri
    ///   scheme or authority, normalization length limits, preservation of OPTIONS
    ///   asterisk-form and rejection of asterisk-form for other methods under every policy.
    #[test]
    fn test_request_uri() {
        let mut pseudo = h2::frame::PseudoHeaders::default();
        assert!(request_uri(&pseudo, TargetPolicy::Raw).is_none());

        // CONNECT uses the authority form
        pseudo.method = Some(Method::CONNECT);
        pseudo.authority = Some("example.com:443".into());
        let target = request_uri(&pseudo, TargetPolicy::Normalize).unwrap();
        assert_eq!(target.method, Method::CONNECT);
        assert_eq!(target.uri.authority().unwrap(), "example.com:443");
        assert!(target.scheme.is_none());
        assert!(target.authority.is_none());

        for authority in ["bad authority", "u@example.com:443", ":443"] {
            pseudo.authority = Some(authority.into());
            assert!(!valid_target(&pseudo), "{authority}");
        }

        // inline and heap buffers
        for authority in ["a.io:1", "very-long-host-name.example.com:8443"] {
            pseudo.authority = Some(authority.into());
            let uri = request_uri(&pseudo, TargetPolicy::Raw).unwrap().uri;
            assert_eq!(uri.authority().unwrap(), authority);
            assert_eq!(uri.to_string(), format!("//{authority}"));
        }

        // other methods require the scheme, the uri holds only the path
        pseudo.method = Some(Method::GET);
        pseudo.authority = Some("example.com".into());
        pseudo.path = Some("/path".into());
        assert!(request_uri(&pseudo, TargetPolicy::Raw).is_none());
        pseudo.scheme = Some("https".into());
        let target = request_uri(&pseudo, TargetPolicy::Normalize).unwrap();
        assert_eq!(target.method, Method::GET);
        assert_eq!(target.uri.as_str(), "/path");
        assert!(target.uri.scheme_str().is_none());
        assert!(target.uri.authority().is_none());
        assert_eq!(target.scheme.as_deref(), Some("https"));
        assert_eq!(target.authority.as_deref(), Some("example.com"));
        // userinfo and an empty host are kept raw but fail validation
        for authority in ["u:p@example.com", ""] {
            pseudo.authority = Some(authority.into());
            let target = request_uri(&pseudo, TargetPolicy::Raw).unwrap();
            assert_eq!(target.authority.as_deref(), Some(authority));
            assert!(!valid_target(&pseudo), "{authority}");
        }
        pseudo.authority = Some("example.com".into());

        pseudo.path = Some("/api/v1/users/1?fields=name,email".into());
        let uri = request_uri(&pseudo, TargetPolicy::Raw).unwrap().uri;
        assert_eq!(uri.path(), "/api/v1/users/1");
        assert_eq!(uri.path_and_query(), "/api/v1/users/1?fields=name,email");

        // target components are normalized only when enabled
        pseudo.scheme = Some("HTTPS".into());
        pseudo.authority = Some("Example.COM:00080".into());
        pseudo.path = Some("/a/./b/../c".into());
        for policy in [TargetPolicy::Raw, TargetPolicy::Validate] {
            let target = request_uri(&pseudo, policy).unwrap();
            assert_eq!(target.uri.as_str(), "/a/./b/../c");
            assert_eq!(target.scheme.as_deref(), Some("HTTPS"));
            assert_eq!(target.authority.as_deref(), Some("Example.COM:00080"));
        }
        let target = request_uri(&pseudo, TargetPolicy::Normalize).unwrap();
        assert_eq!(target.uri.as_str(), "/a/c");
        assert_eq!(target.scheme.as_deref(), Some("https"));
        assert_eq!(target.authority.as_deref(), Some("example.com:80"));
        pseudo.authority = Some("example.com".into());
        pseudo.path = Some("/path".into());

        pseudo.scheme = Some("ht/tp".into());
        assert!(!valid_target(&pseudo));
        pseudo.scheme = Some("https".into());
        pseudo.authority = Some("example.com/x?".into());
        assert!(!valid_target(&pseudo));
        pseudo.authority = Some("example.com".into());

        // a `:path` that is not origin-form never supplies the uri scheme or authority,
        // a colon in the first segment is guarded by `./`, see RFC 3986 section 4.2
        for (path, uri_path) in [
            ("x", "x"),
            ("http://evil.com/x", "./http://evil.com/x"),
            ("evil.com:80", "./evil.com:80"),
        ] {
            pseudo.path = Some(path.into());
            let target = request_uri(&pseudo, TargetPolicy::Raw).unwrap();
            assert_eq!(target.uri.path(), uri_path);
            assert!(target.uri.scheme_str().is_none(), "{path}");
            assert!(target.uri.authority().is_none(), "{path}");
            assert_eq!(target.authority.as_deref(), Some("example.com"));
            assert!(!valid_target(&pseudo), "{path}");
        }

        pseudo.authority = None;
        pseudo.path = Some("/path".into());
        let target = request_uri(&pseudo, TargetPolicy::Normalize).unwrap();
        assert_eq!(target.uri.path(), "/path");
        assert!(target.authority.is_none());
        pseudo.path = Some("//example.com/path".into());
        let uri = request_uri(&pseudo, TargetPolicy::Normalize).unwrap().uri;
        assert_eq!(uri.path(), "//example.com/path");
        assert!(uri.authority().is_none());
        pseudo.path = Some("/a b".into());
        assert!(!valid_target(&pseudo));
        pseudo.path = Some("/a\x7f".into());
        assert!(!valid_target(&pseudo));
        for path in ["/a#f", "/a%zz", "/a\"", "/a%00"] {
            pseudo.path = Some(path.into());
            assert!(!valid_target(&pseudo), "{path}");
        }
        pseudo.path = Some("/a%3b/%2e%2e/b?%00".into());
        let uri = request_uri(&pseudo, TargetPolicy::Raw).unwrap().uri;
        assert_eq!(uri.path_and_query(), "/a%3b/%2e%2e/b?%00");
        let uri = request_uri(&pseudo, TargetPolicy::Normalize).unwrap().uri;
        assert_eq!(uri.path_and_query(), "/b?%00");

        pseudo.path = Some(format!("/{}", "[".repeat(22000)).into());
        assert!(request_uri(&pseudo, TargetPolicy::Raw).is_some());
        assert!(request_uri(&pseudo, TargetPolicy::Normalize).is_none());

        pseudo.path = Some("*".into());
        for policy in [
            TargetPolicy::Raw,
            TargetPolicy::Validate,
            TargetPolicy::Normalize,
        ] {
            assert!(request_uri(&pseudo, policy).is_none());
        }
        pseudo.method = Some(Method::OPTIONS);
        let uri = request_uri(&pseudo, TargetPolicy::Normalize).unwrap().uri;
        assert_eq!(uri.path(), "*");
        assert_eq!(uri.as_str(), "*");
    }

    /// STATEMENT: HTTP/2 preserves authority boundaries for opt-in transport validation.
    ///
    /// HOW:
    /// - Preserve malformed authorities with processing disabled and reject them when enabled.
    /// - Accept valid authorities while preserving their case, escapes, port spelling and
    ///   boundaries, in the uri for CONNECT and beside the origin-form uri otherwise.
    #[test]
    fn test_request_uri_authority() {
        for (method, path) in [
            (Method::GET, Some("/path")),
            (Method::CONNECT, None),
            (Method::OPTIONS, Some("*")),
        ] {
            let mut pseudo = h2::frame::PseudoHeaders {
                method: Some(method),
                scheme: path.map(|_| "https".into()),
                path: path.map(Into::into),
                ..Default::default()
            };
            // returns the uri and the authority
            let target = |pseudo: &h2::frame::PseudoHeaders, policy| {
                let target = request_uri(pseudo, policy).unwrap();
                let authority = if path.is_some() {
                    assert!(target.uri.authority().is_none());
                    target.authority.unwrap().to_string()
                } else {
                    assert!(target.authority.is_none());
                    target.uri.authority().unwrap().as_str().to_owned()
                };
                (target.uri.to_string(), authority)
            };
            for authority in [
                "",
                ":",
                ":443",
                "@example.com",
                "u:p@example.com:443",
                "example.com@",
                "example.com@other.com",
                "bad authority",
                "exa\tmple.com",
                "m\u{fc}nchen.de",
                "example.com/path",
                "example.com?query",
                "example.com#fragment",
                "example.com%zz",
                "example.com:abc",
                "example.com:+443",
                "example.com:65536",
                "127.0.0.256",
                "::1",
                "[::1",
                "[::g]",
                "[::1]:65536",
            ] {
                pseudo.authority = Some(authority.into());
                assert_eq!(target(&pseudo, TargetPolicy::Raw).1, authority);
                assert!(!valid_target(&pseudo), "{pseudo:?}");
            }
            for authority in [
                "example.com",
                "Example.COM:00080",
                "example.com:",
                "example.com:0",
                "example.com:65535",
                "%45xample.com:443",
                "127.0.0.1:8443",
                "[::1]",
                "[0:0:0:0:0:0:0:1]:00443",
            ] {
                pseudo.authority = Some(authority.into());
                let expected = (
                    path.map_or_else(|| format!("//{authority}"), ToOwned::to_owned),
                    authority.to_owned(),
                );
                assert_eq!(target(&pseudo, TargetPolicy::Raw), expected, "{pseudo:?}");
                assert_eq!(
                    target(&pseudo, TargetPolicy::Validate),
                    expected,
                    "{pseudo:?}"
                );
                assert!(valid_target(&pseudo), "{pseudo:?}");
            }
        }
    }

    /// STATEMENT: HTTP/2 `:scheme` and `:authority` checks agree with building a url from
    /// them and calling `Url::validate_target` and `Url::normalize`.
    ///
    /// HOW:
    /// - Combine valid, invalid, non-canonical, encoded, IPv4 and IPv6 values.
    /// - Compare the scheme and authority of `request_uri` under `Validate` and `Normalize`
    ///   with the url built, validated and optionally normalized from the same values.
    #[test]
    fn test_request_uri_scheme_authority() {
        let reference = |scheme: &str, authority: Option<&str>, normalize: bool| {
            let mut url = Url::from_target_parts(Some(scheme), authority, "/").ok()?;
            url.validate_target().ok()?;
            if normalize {
                url.normalize().ok()?;
            }
            let authority = url.authority().map(|a| a.as_str().to_owned());
            Some((url.scheme_str()?.to_owned(), authority))
        };
        for scheme in ["https", "HTTPS", "http", "h2+x.y-z", "1http", "ht/tp", ""] {
            for authority in [
                None,
                Some("example.com"),
                Some("Example.COM:00080"),
                Some("example.com:"),
                Some("example.com:0"),
                Some("example.com:0443"),
                Some("example.com:443"),
                Some("example.com:80"),
                Some("example.com:8443"),
                Some("example.com:65536"),
                Some("127.0.0.1:8080"),
                Some("127.0.0.256"),
                Some("[::1]:443"),
                Some("[0:0::1]"),
                Some("[::1"),
                Some("%65xample.com"),
                Some("%45xample.com:0080"),
                Some("example.com%zz"),
                Some("u@example.com"),
                Some(""),
                Some(":443"),
                Some("bad host"),
            ] {
                let pseudo = h2::frame::PseudoHeaders {
                    method: Some(Method::GET),
                    scheme: Some(scheme.into()),
                    authority: authority.map(Into::into),
                    path: Some("/".into()),
                    ..Default::default()
                };
                for policy in [TargetPolicy::Validate, TargetPolicy::Normalize] {
                    let target = request_uri(&pseudo, policy).map(|t| {
                        (
                            t.scheme.unwrap().to_string(),
                            t.authority.map(|a| a.to_string()),
                        )
                    });
                    let expected = reference(scheme, authority, policy == TargetPolicy::Normalize);
                    assert_eq!(target, expected, "{scheme:?} {authority:?} {policy:?}");
                }
            }
        }
    }

    /// STATEMENT: HTTP/2 responses drop connection-specific headers, get a content-length
    /// that matches the body size, and keep or add a `date` header.
    ///
    /// HOW:
    /// - For each of the five connection-specific headers, prepare a response with only
    ///   that header plus a custom header, a stale `content-length` and a `date`; check
    ///   that only the custom header, the original `date` and the sized
    ///   `content-length` remain.
    /// - Prepare `Empty`, streaming and `204` sized responses without a `date`; check the
    ///   `content-length` is `0`, removed and removed, the size becomes `None` for `204`,
    ///   and a `date` is added.
    #[crate::rt_test]
    async fn test_prepare_response() {
        for name in [
            header::CONNECTION,
            header::TRANSFER_ENCODING,
            header::UPGRADE,
            KEEP_ALIVE,
            PROXY_CONNECTION,
        ] {
            let mut head = ResponseHead::new(StatusCode::OK, Version::HTTP_2);
            let hdrs = head.headers_mut();
            hdrs.insert(name.clone(), HeaderValue::from_static("value"));
            hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from_static("100"));
            hdrs.insert(header::DATE, HeaderValue::from_static("custom"));
            hdrs.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

            let mut size = BodySize::Sized(5);
            prepare_response(&mut head, &mut size);
            assert_eq!(size, BodySize::Sized(5));
            assert!(!head.headers.contains_key(&name), "{name}");
            assert_eq!(head.headers.len(), 3, "{name}");
            assert_eq!(head.headers.get(header::CONTENT_LENGTH).unwrap(), "5");
            assert_eq!(head.headers.get(header::DATE).unwrap(), "custom");
            assert_eq!(
                head.headers.get(header::CONTENT_TYPE).unwrap(),
                "text/plain"
            );
        }

        for (status, mut size, len) in [
            (StatusCode::OK, BodySize::Empty, Some("0")),
            (StatusCode::OK, BodySize::Stream, None),
            (StatusCode::NO_CONTENT, BodySize::Sized(5), None),
        ] {
            let mut head = ResponseHead::new(status, Version::HTTP_2);
            head.headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from_static("100"));
            prepare_response(&mut head, &mut size);
            if status == StatusCode::NO_CONTENT {
                assert_eq!(size, BodySize::None);
            }
            assert_eq!(
                head.headers
                    .get(header::CONTENT_LENGTH)
                    .map(|v| v.to_str().unwrap()),
                len,
                "{status}"
            );
            assert!(head.headers.contains_key(header::DATE), "{status}");
        }
    }
}
