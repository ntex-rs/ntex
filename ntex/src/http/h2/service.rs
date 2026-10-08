use std::{cell::Cell, cell::RefCell, future::poll_fn, io, mem, rc::Rc};

use ntex_h2::{self as h2, control::ExpectResult, frame::StreamId, server};
use urly::{Authority, Scheme, Url};

use crate::error::{Error, IntoFailure};
use crate::http::body::{Body, BodySize, MessageBody, ResponseBody};
use crate::http::config::DispatcherConfig;
use crate::http::error::{DispatchError, H2Error, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::helpers::is_valid_target;
use crate::http::message::{CurrentIo, ResponseHead};
use crate::http::{DateService, Method, Request, Response, StatusCode, Version};
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
struct ControlService {
    inner: Pipeline<h2::Control<Error<H2Error>>, h2::ControlAck, DispatchError>,
}

impl Service<(), h2::Control<Error<H2Error>>> for ControlService {
    type Res = h2::ControlAck;
    type Error = DispatchError;

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
        self.inner.ready().await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.inner.shutdown().await;
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
        self.inner.call(msg).await
    }
}

struct PublishService<Err> {
    id: usize,
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
        Self {
            id,
            io,
            svc,
            control,
            streams: Rc::new(RefCell::new(HashMap::default())),
            empty_data: Cell::new(0),
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
                return Ok(());
            }
            h2::MessageKind::Eof(item) => {
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
                if let Some(StreamPayload { sender, .. }) =
                    self.streams.borrow_mut().remove(&stream.id())
                {
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

        // a malformed request is a stream error, the connection stays open,
        // see RFC 9113 section 8.1.1
        let Some((method, uri)) = request_uri(&pseudo) else {
            log::debug!(
                "{}: Malformed request on {:?}: {pseudo:?}",
                self.io.tag(),
                stream.id()
            );
            self.streams.borrow_mut().remove(&stream.id());

            let mut res = Response::new(StatusCode::BAD_REQUEST).drop_body();
            let head = res.head_mut();
            prepare_response(head, &mut BodySize::Empty);
            let _ = stream.send_response(StatusCode::BAD_REQUEST, &head.headers, true);
            if !eof {
                // the request body is not needed
                stream.reset(h2::frame::Reason::NO_ERROR);
            }
            return Ok(());
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
                    h2::recycle_header_map(mem::replace(&mut head.headers, headers));
                    prepare_response(head, &mut BodySize::Empty);
                    let _ = stream.send_response(status, &head.headers, true);

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

        let is_head_req = method == Method::HEAD;
        let head = req.head_mut();
        head.uri = uri;
        head.version = Version::HTTP_2;
        head.method = method;
        // the pooled head map is empty, the h2 decoder reuses its allocation
        h2::recycle_header_map(mem::replace(&mut head.headers, headers));
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

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_CONTENT_LENGTH: HeaderValue = HeaderValue::from_static("0");
#[allow(clippy::declare_interior_mutable_const)]
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");
#[allow(clippy::declare_interior_mutable_const)]
const PROXY_CONNECTION: HeaderName = HeaderName::from_static("proxy-connection");

/// Builds the request method and uri from the pseudo headers,
/// returns `None` for a malformed request.
fn request_uri(pseudo: &h2::frame::PseudoHeaders) -> Option<(Method, Url)> {
    let method = pseudo.method.clone()?;
    // userinfo is deprecated, see RFC 9113 section 8.3.1
    let authority = match pseudo.authority {
        Some(ref authority) => {
            // An empty host is "" or ":port"; IPv6 hosts are bracketed.
            if authority.is_empty() || authority.starts_with(':') || authority.contains('@') {
                return None;
            }
            Some(Authority::new(authority.as_str()).ok()?)
        }
        None => None,
    };
    let path = pseudo.path.as_ref().map(crate::util::ByteString::as_str);
    // the same request-target checks as http/1, `Url` parsing is lenient
    if path.is_some_and(|p| !is_valid_target(p.as_bytes())) {
        return None;
    }

    let uri = match (authority, path) {
        // CONNECT request uses the authority form
        (Some(authority), None) if method == Method::CONNECT => {
            Url::parse(authority.as_str()).ok()?
        }
        (_, Some("*")) if method == Method::OPTIONS => Url::from_static("*"),
        (Some(authority), Some(path)) if path.starts_with('/') => {
            let scheme = Scheme::new(pseudo.scheme.as_ref()?.as_str()).ok()?;
            Url::try_from(concat(&[scheme.as_str(), "://", authority.as_str(), path])).ok()?
        }
        (None, Some(path)) if path.starts_with('/') => {
            let uri = if path.starts_with("//") {
                // origin-form path, not a network-path reference
                Url::try_from(format!("/.{path}")).ok()?
            } else {
                // reuses the buffer if the path is normalized
                Url::try_from(pseudo.path.as_ref()?).ok()?
            };
            if uri.authority().is_some() {
                return None;
            }
            uri
        }
        _ => return None,
    };
    Some((method, uri))
}

/// Concatenates `pieces` into a buffer that `Url` reuses if the result is
/// normalized, short strings are stored inline without allocation.
fn concat(pieces: &[&str]) -> Bytes {
    const INLINE: usize = 23;

    let len = pieces.iter().map(|p| p.len()).sum::<usize>();
    if len <= INLINE {
        let mut buf = [0u8; INLINE];
        let mut pos = 0;
        for p in pieces {
            buf[pos..pos + p.len()].copy_from_slice(p.as_bytes());
            pos += p.len();
        }
        Bytes::copy_from_slice(&buf[..len])
    } else {
        let mut buf = BytesMut::with_capacity(len);
        for p in pieces {
            buf.extend_from_slice(p.as_bytes());
        }
        buf.freeze()
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
    match size {
        BodySize::None | BodySize::Stream => head.headers.remove(header::CONTENT_LENGTH),
        BodySize::Empty => head
            .headers
            .insert(header::CONTENT_LENGTH, ZERO_CONTENT_LENGTH),
        BodySize::Sized(len) => {
            head.headers
                .insert(header::CONTENT_LENGTH, HeaderValue::from(*len));
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
        head.headers
            .insert(header::DATE, DateService::header_value());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_uri() {
        let mut pseudo = h2::frame::PseudoHeaders::default();
        assert!(request_uri(&pseudo).is_none());

        // CONNECT uses the authority form
        pseudo.method = Some(Method::CONNECT);
        pseudo.authority = Some("example.com:443".into());
        let (method, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(method, Method::CONNECT);
        assert_eq!(uri.authority().unwrap(), "example.com:443");

        for authority in ["bad authority", "u@example.com:443", ":443"] {
            pseudo.authority = Some(authority.into());
            assert!(request_uri(&pseudo).is_none(), "{authority}");
        }

        // inline and heap buffers
        for authority in ["a.io:1", "very-long-host-name.example.com:8443"] {
            pseudo.authority = Some(authority.into());
            let (_, uri) = request_uri(&pseudo).unwrap();
            assert_eq!(uri.authority().unwrap(), authority);
            assert_eq!(uri.to_string(), format!("//{authority}"));
        }

        // absolute form requires the scheme
        pseudo.method = Some(Method::GET);
        pseudo.authority = Some("example.com".into());
        pseudo.path = Some("/path".into());
        assert!(request_uri(&pseudo).is_none());
        pseudo.scheme = Some("https".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.to_string(), "https://example.com/path");
        // userinfo and an empty host are rejected
        for authority in ["u:p@example.com", ""] {
            pseudo.authority = Some(authority.into());
            assert!(request_uri(&pseudo).is_none(), "{authority}");
        }
        pseudo.authority = Some("example.com".into());

        // 23 bytes are stored inline, 24 bytes on the heap
        for path in ["/123", "/1234", "/api/v1/users/1?fields=name,email"] {
            pseudo.path = Some(path.into());
            let (_, uri) = request_uri(&pseudo).unwrap();
            assert_eq!(uri.to_string(), format!("https://example.com{path}"));
        }
        // the buffer is normalized if needed
        pseudo.scheme = Some("HTTPS".into());
        pseudo.authority = Some("Example.COM".into());
        pseudo.path = Some("/a/./b/../c".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.to_string(), "https://example.com/a/c");
        pseudo.path = Some("/a/./b/../c/long-enough-for-the-heap".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(
            uri.to_string(),
            "https://example.com/a/c/long-enough-for-the-heap"
        );
        pseudo.authority = Some("example.com".into());
        pseudo.path = Some("/path".into());

        pseudo.scheme = Some("ht/tp".into());
        assert!(request_uri(&pseudo).is_none());
        pseudo.scheme = Some("https".into());
        pseudo.authority = Some("example.com/x?".into());
        assert!(request_uri(&pseudo).is_none());
        pseudo.authority = Some("example.com".into());
        pseudo.path = Some("x".into());
        assert!(request_uri(&pseudo).is_none());

        pseudo.authority = None;
        pseudo.path = Some("/path".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.path(), "/path");
        pseudo.path = Some("//example.com/path".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.path(), "//example.com/path");
        assert!(uri.authority().is_none());
        pseudo.path = Some("/a b".into());
        assert!(request_uri(&pseudo).is_none());
        pseudo.path = Some("/a\x7f".into());
        assert!(request_uri(&pseudo).is_none());
        for path in ["/a#f", "/a%zz", "/a\"", "/a%00"] {
            pseudo.path = Some(path.into());
            assert!(request_uri(&pseudo).is_none(), "{path}");
        }
        pseudo.path = Some("/a%3b/%2e%2e/b?%00".into());
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.path_and_query(), "/b?%00");

        pseudo.path = Some("*".into());
        assert!(request_uri(&pseudo).is_none());
        pseudo.method = Some(Method::OPTIONS);
        let (_, uri) = request_uri(&pseudo).unwrap();
        assert_eq!(uri.path(), "*");
    }

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
                assert!(request_uri(&pseudo).is_none(), "{pseudo:?}");
            }
            for (authority, normalized) in [
                ("example.com", "example.com"),
                ("Example.COM:00080", "example.com:80"),
                ("example.com:", "example.com"),
                ("example.com:0", "example.com:0"),
                ("example.com:65535", "example.com:65535"),
                ("%45xample.com:443", "example.com:443"),
                ("127.0.0.1:8443", "127.0.0.1:8443"),
                ("[::1]", "[::1]"),
                ("[0:0:0:0:0:0:0:1]:00443", "[::1]:443"),
            ] {
                pseudo.authority = Some(authority.into());
                let (_, uri) = request_uri(&pseudo).unwrap();
                let expected = match path {
                    None => format!("//{normalized}"),
                    Some("*") => "*".to_owned(),
                    _ => format!("https://{normalized}/path"),
                };
                assert_eq!(uri.as_str(), expected, "{pseudo:?}");
            }
        }
    }

    #[test]
    fn test_concat() {
        assert_eq!(concat(&[]), "");
        assert_eq!(concat(&["//", "a.io"]), "//a.io");
        let inline = "x".repeat(23);
        assert_eq!(concat(&[&inline[..20], &inline[20..]]), inline.as_str());
        let heap = "y".repeat(24);
        assert_eq!(concat(&[&heap[..1], "", &heap[1..]]), heap.as_str());
    }
}
