use std::{cell::RefCell, future::poll_fn, io, mem, rc::Rc};

use ntex_h2::{self as h2, control::ExpectResult, frame::StreamId, server};

use crate::error::{Error, IntoFailure};
use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Uri, Version};
use crate::io::{Filter, Io, IoBoxed, IoRef, types};
use crate::service::pipeline::{Pipeline, PipelineBinding, PipelineFactory};
use crate::service::{Ctx, IntoServiceFactory, RequestState, Service, ServiceFactory};
use crate::util::{Bytes, BytesMut, HashMap};

use super::{DefaultControlService, payload::Payload, payload::PayloadSender};

/// An HTTP/2 transport service.
#[derive(derive_more::Debug)]
#[debug("H2Service")]
pub struct H2Service<F, Req: RequestState<Io<F>>, Err> {
    sf: crate::http::HttpPipeline<Req::State, Err>,
    ctl: crate::http::Ctl2Pipeline<Req::State>,
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
            ctl: PipelineFactory::new(DefaultControlService),
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
    pub fn control<I, Sf>(self, ctl: I) -> Self
    where
        I: IntoServiceFactory<Sf, Req::State, h2::Control<Error<H2Error>>>,
        Sf: ServiceFactory<Req::State, h2::Control<Error<H2Error>>, Res = h2::ControlAck> + 'static,
        Sf::Error: IntoFailure,
        Sf::InitError: IntoFailure,
    {
        H2Service {
            sf: self.sf,
            ctl: PipelineFactory::new(
                ctl.into_factory()
                    .map_err(|e| DispatchError::Service(e.fail()))
                    .map_init_err(|e| DispatchError::Service(e.fail())),
            ),
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
        let ctl = self.ctl.create(st).await?;

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
    control: Pipeline<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>,
) -> Result<(), DispatchError>
where
    Err: ResponseError + 'static,
{
    let ioref = io.get_ref();

    let _ = server::handle_one(
        io,
        Pipeline::new((), PublishService::new(id, ioref, svc, control.bind())),
        control.bind(),
    )
    .await;

    Ok(())
}

struct PublishService<Err> {
    id: usize,
    io: IoRef,
    svc: Pipeline<Request, Response, Err>,
    control: PipelineBinding<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>,
    streams: Rc<RefCell<HashMap<StreamId, StreamPayload>>>,
}

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
        Self {
            id,
            io,
            svc,
            control,
            streams: Rc::new(RefCell::new(HashMap::default())),
        }
    }
}

impl<Err> Service<(), h2::Message> for PublishService<Err>
where
    Err: ResponseError + 'static,
{
    type Res = ();
    type Error = Error<H2Error>;

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
                    self.streams.borrow_mut().insert(
                        stream.id(),
                        StreamPayload {
                            sender,
                            complete: false,
                        },
                    );
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
                return Ok(());
            }
            h2::MessageKind::Eof(item) => {
                log::debug!(
                    "{}: Got payload eof for {:?}: {item:?}",
                    self.io.tag(),
                    stream.id()
                );
                if let Some(StreamPayload { sender, .. }) =
                    self.streams.borrow_mut().remove(&stream.id())
                {
                    match item {
                        h2::StreamEof::Data(data, cap) => {
                            sender.feed_eof(data, Some(cap));
                        }
                        h2::StreamEof::Trailers(_) => {
                            sender.feed_eof(Bytes::new(), None);
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
                if let Some(pl) = self.streams.borrow_mut().remove(&stream.id()) {
                    pl.sender
                        .set_error(io::Error::new(io::ErrorKind::UnexpectedEof, err).into());
                }
                return Ok(());
            }
        };

        // the client waits for `100 Continue` before sending the request body,
        // see RFC 9110 section 10.1.1
        let (pseudo, headers) = if !eof && expect_continue(&headers) {
            let msg = h2::Control::expect(stream.clone(), pseudo, headers);
            match self
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
                        return Ok(());
                    }
                    let (_, pseudo, headers) = expect.into_parts();
                    (pseudo, headers)
                }
                Ok(Some(ExpectResult::Failed(_, status, headers))) => {
                    self.streams.borrow_mut().remove(&stream.id());

                    let mut res = Response::new(status).drop_body();
                    let head = res.head_mut();
                    head.headers = headers;
                    prepare_response(head, &mut BodySize::Empty);
                    let hdrs = mem::replace(&mut head.headers, HeaderMap::new());
                    let _ = stream.send_response(status, hdrs, true);

                    // the request body is not needed
                    stream.reset(h2::frame::Reason::NO_ERROR);
                    return Ok(());
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
                    return Ok(());
                }
            }
        } else {
            (pseudo, headers)
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

        let method = pseudo.method.ok_or(H2Error::MissingPseudo("Method"))?;

        let head = req.head_mut();
        head.uri = if method == Method::CONNECT
            && pseudo.path.is_none()
            && let Some(ref authority) = pseudo.authority
        {
            // CONNECT request uses the authority form
            Uri::try_from(authority.as_str()).map_err(Error::from_err)?
        } else if let Some(ref authority) = pseudo.authority {
            let path = pseudo.path.ok_or(H2Error::MissingPseudo("Path"))?;
            let scheme = pseudo.scheme.ok_or(H2Error::MissingPseudo("Scheme"))?;
            Uri::try_from(format!("{scheme}://{authority}{path}")).map_err(Error::from_err)?
        } else {
            let path = pseudo.path.ok_or(H2Error::MissingPseudo("Path"))?;
            Uri::try_from(path.as_str()).map_err(Error::from_err)?
        };
        let is_head_req = method == Method::HEAD;
        head.version = Version::HTTP_2;
        head.method = method;
        head.headers = headers;
        head.io = CurrentIo::Ref(io);
        head.id = self.id;

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

        let hdrs = mem::replace(&mut head.headers, HeaderMap::new());
        if size.is_eof() || is_head_req {
            stream
                .send_response(head.status, hdrs, true)
                .map_err(Error::map_err)?;
        } else {
            stream
                .send_response(head.status, hdrs, false)
                .map_err(Error::map_err)?;

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
                            .map_err(Error::map_err)?;
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
                                .map_err(Error::map_err)?;
                        }
                    }
                    Some(Err(e)) => {
                        #[cfg(feature = "trace")]
                        log::error!("{}: Response payload stream error: {e:?}", self.io.tag());
                        return Err(H2Error::Stream(e).into());
                    }
                }
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

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_CONTENT_LENGTH: HeaderValue = HeaderValue::from_static("0");
#[allow(clippy::declare_interior_mutable_const)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
#[allow(clippy::declare_interior_mutable_const)]
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

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
    if head.status == StatusCode::NO_CONTENT {
        *size = BodySize::None;
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
