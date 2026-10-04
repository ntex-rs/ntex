use std::{cell::Cell, future::poll_fn, io, rc::Rc, task::Poll, time::Instant};

use ntex_h2::client::{RecvStream, SimpleClient, StreamReservation};
use ntex_h2::{self as h2, frame};

use crate::error::{Error, ErrorMapping, with_service};
use crate::http::body::{Body, BodySize, MessageBody};
use crate::http::header::{self, HeaderMap, HeaderValue};
use crate::http::{Method, Payload, ResponseHead, Version, h2::Payload as H2Payload};
use crate::time::{Millis, now, timeout_checked};
use crate::util::{ByteString, Bytes, BytesMut, Either, select};

use super::{ClientRawRequest, error::ClientError, error::ConnectError};

pub(super) async fn send_request(
    client: H2Client,
    req: ClientRawRequest,
    body: Body,
    timeout: Millis,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    with_service(
        client.service(),
        send_request_inner(client, req, body, timeout),
    )
    .await
}

async fn send_request_inner(
    mut client: H2Client,
    req: ClientRawRequest,
    body: Body,
    timeout: Millis,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    #[cfg(feature = "trace")]
    log::trace!(
        "{}: Sending client request: {req:?} {:?}",
        client.client.tag(),
        body.size()
    );
    let length = body.size();
    let eof = if req.head.method == Method::HEAD {
        true
    } else {
        matches!(
            length,
            BodySize::None | BodySize::Empty | BodySize::Sized(0)
        )
    };

    let mut hdrs = h2_headers(&req);

    // Content length
    match length {
        BodySize::None | BodySize::Stream => (),
        BodySize::Empty => {
            hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        }
        BodySize::Sized(len) => {
            let mut buf = BytesMut::new();
            crate::http::h1::encoder::convert_usize(len, &mut buf, false);

            hdrs.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_shared(buf.freeze()).unwrap(),
            );
        }
    }

    // send request
    let uri = &req.head.uri;
    let path = match uri.path_and_query().as_str() {
        "" => ByteString::from_static("/"),
        path => ByteString::from(path),
    };
    let method = req.head.method.clone();
    let res = if let Some(reservation) = client.reservation.take() {
        reservation.send(method, path, hdrs, eof)
    } else {
        client.client.send(method, path, hdrs, eof).await
    };
    let (snd_stream, rcv_stream) = res.into_error()?;

    // send body
    if !eof {
        // sending body is async process, we can handle upload and download
        // at the same time
        let activity = client.activity.clone();
        crate::rt::spawn(async move {
            let _activity = activity;
            if let Err(e) = send_body(body, &snd_stream).await {
                log::debug!("{}: Cannot send body: {e:?}", snd_stream.tag());
                snd_stream.reset(frame::Reason::INTERNAL_ERROR);
            }
        });
    }

    timeout_checked(timeout, get_response(rcv_stream, client.activity.clone()))
        .await
        .map_err(|()| Error::from(ClientError::Timeout))
        .and_then(|res| res)
}

static TRAILERS: HeaderValue = HeaderValue::from_static("trailers");

/// Merges request head and extra headers.
fn h2_headers(req: &ClientRawRequest) -> HeaderMap {
    let empty = HeaderMap::new();
    let extra_headers = req.headers.as_ref().unwrap_or(&empty);
    let mut hdrs = HeaderMap::new();
    for (name, value) in req
        .head
        .headers
        .iter()
        .filter(|(name, _)| !extra_headers.contains_key(*name))
        .chain(extra_headers.iter())
        .filter(|(name, value)| is_h2_header(name, value))
    {
        let value = if *name == header::TE { &TRAILERS } else { value };
        hdrs.append(name.clone(), value.clone());
    }
    hdrs
}

/// Returns `false` for connection-specific header fields, they are not used by
/// HTTP/2 (RFC 9113 §8.2.2), `:authority` is used instead of `Host` (RFC 9113 §8.3.1)
fn is_h2_header(name: &header::HeaderName, value: &HeaderValue) -> bool {
    match *name {
        header::CONNECTION | header::TRANSFER_ENCODING | header::UPGRADE | header::HOST => false,
        header::TE => value.as_bytes().eq_ignore_ascii_case(b"trailers"),
        _ => !(name == "keep-alive" || name == "proxy-connection"),
    }
}

async fn get_response(
    rcv_stream: RecvStream,
    activity: Option<H2Activity>,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    let h2::Message { stream, kind } = loop {
        let msg = rcv_stream
            .recv()
            .await
            .ok_or(ClientError::Connect(ConnectError::Disconnected(None)))?;

        // skip interim responses
        if let h2::MessageKind::Headers { ref pseudo, .. } = msg.kind
            && pseudo.status.is_some_and(|s| s.is_informational())
        {
            log::trace!("Skipping interim response: {:?}", pseudo.status);
            continue;
        }
        break msg;
    };

    match kind {
        h2::MessageKind::Headers {
            pseudo,
            headers,
            eof,
        } => {
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: {:?} got response (eof: {eof}): {pseudo:#?}\nheaders: {headers:#?}",
                stream.tag(),
                stream.id(),
            );

            match pseudo.status {
                Some(status) => {
                    let mut head = ResponseHead::new(status, Version::HTTP_2);
                    head.headers = headers;

                    let payload = if eof {
                        Payload::None
                    } else {
                        #[cfg(feature = "trace")]
                        log::debug!(
                            "{}: Creating local payload stream for {:?}",
                            stream.tag(),
                            stream.id()
                        );
                        let (pl, payload) = H2Payload::create(stream.empty_capacity());

                        crate::rt::spawn(async move {
                            let _activity = activity;
                            loop {
                                #[allow(unused_variables)]
                                let h2::Message { stream, kind } = match select(
                                    rcv_stream.recv(),
                                    poll_fn(|cx| pl.on_cancel(cx.waker())),
                                )
                                .await
                                {
                                    Either::Left(Some(msg)) => msg,
                                    Either::Left(None) => {
                                        pl.feed_eof(Bytes::new(), None);
                                        break;
                                    }
                                    Either::Right(()) => break,
                                };

                                match kind {
                                    h2::MessageKind::Data(data, cap) => {
                                        #[cfg(feature = "trace")]
                                        log::trace!(
                                            "{}: Got data chunk for {:?}: {:?}",
                                            stream.tag(),
                                            stream.id(),
                                            data.len()
                                        );
                                        pl.feed_data(data, cap);
                                    }
                                    h2::MessageKind::Eof(item) => {
                                        #[cfg(feature = "trace")]
                                        log::trace!(
                                            "{}: Got payload eof for {:?}: {item:?}",
                                            stream.tag(),
                                            stream.id(),
                                        );
                                        match item {
                                            h2::StreamEof::Data(data, cap) => {
                                                pl.feed_eof(data, Some(cap));
                                            }
                                            h2::StreamEof::Trailers(hdrs) => {
                                                pl.feed_trailers(hdrs);
                                            }
                                            h2::StreamEof::Error(err) => {
                                                pl.set_error(err.into_error().into());
                                            }
                                        }
                                    }
                                    h2::MessageKind::Disconnect(err) => {
                                        #[cfg(feature = "trace")]
                                        log::trace!(
                                            "{}: Connection is disconnected {err:?}",
                                            stream.tag(),
                                        );
                                        pl.set_error(
                                            io::Error::new(io::ErrorKind::UnexpectedEof, err)
                                                .into(),
                                        );
                                    }
                                    h2::MessageKind::Headers { .. } => {
                                        pl.set_error(
                                            io::Error::new(
                                                io::ErrorKind::Unsupported,
                                                "Unexpected h2 message",
                                            )
                                            .into(),
                                        );
                                        break;
                                    }
                                }
                            }
                        });
                        Payload::H2(payload)
                    };
                    Ok((head, payload))
                }
                None => Err(Error::from(ClientError::H2(h2::OperationError::Stream(
                    h2::StreamError::MissingPseudo("status"),
                )))),
            }
        }
        h2::MessageKind::Disconnect(err) => Err(err.map(ClientError::H2)),
        _ => Err(Error::from(ClientError::Error(Rc::new(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unexpected h2 message",
        ))))),
    }
}

async fn send_body(
    mut body: Body,
    stream: &h2::client::SendStream,
) -> Result<(), Error<ClientError>> {
    let reset = stream.on_reset();
    loop {
        let chunk = poll_fn(|cx| {
            // the body can wait for data after the stream is reset
            if stream.is_reset() {
                return Poll::Ready(None);
            }
            while reset.poll_ready(cx).is_ready() {
                if stream.is_reset() {
                    return Poll::Ready(None);
                }
            }
            body.poll_next_chunk(cx).map(Some)
        })
        .await;

        let Some(chunk) = chunk else {
            log::trace!(
                "{}: {:?} stream is reset, stop sending body",
                stream.tag(),
                stream.id()
            );
            return Ok(());
        };

        match chunk {
            Some(Ok(b)) => {
                #[cfg(feature = "trace")]
                log::trace!(
                    "{}: {:?} sending chunk, {} bytes",
                    stream.tag(),
                    stream.id(),
                    b.len()
                );
                stream.send_payload(b, false).await.into_error()?;
            }
            Some(Err(e)) => return Err(Error::from(ClientError::from(e))),
            None => {
                #[cfg(feature = "trace")]
                log::trace!("{}: {:?} eof of send stream ", stream.tag(), stream.id());
                return stream.send_payload(Bytes::new(), true).await.into_error();
            }
        }
    }
}

/// Shared HTTP/2 connection.
///
/// Pool keeps a copy without activity. Copies returned by [`H2Client::begin`]
/// carry a stream reservation, used by the request, and an activity guard
/// that is held until the request, including its request body and response
/// payload, is complete.
pub(super) struct H2Client {
    client: SimpleClient,
    state: Rc<H2State>,
    activity: Option<H2Activity>,
    reservation: Option<StreamReservation>,
}

impl Clone for H2Client {
    /// Stream reservation is not cloned.
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            state: self.state.clone(),
            activity: self.activity.clone(),
            reservation: None,
        }
    }
}

impl std::fmt::Debug for H2Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H2Client")
            .field("streams", &self.streams())
            .field("closed", &self.is_closed())
            .finish()
    }
}

struct H2State {
    created: Cell<Instant>,
    used: Cell<Instant>,
    // number of live activity guards
    guards: Cell<u32>,
}

/// In-flight request guard.
///
/// Every copy is counted, the connection is idle when no copies are left.
pub(super) struct H2Activity(Rc<H2State>);

impl H2Activity {
    fn new(state: &Rc<H2State>) -> Self {
        state.guards.set(state.guards.get() + 1);
        H2Activity(state.clone())
    }
}

impl Clone for H2Activity {
    fn clone(&self) -> Self {
        H2Activity::new(&self.0)
    }
}

impl Drop for H2Activity {
    fn drop(&mut self) {
        let guards = self.0.guards.get() - 1;
        self.0.guards.set(guards);
        if guards == 0 {
            self.0.used.set(now());
        }
    }
}

impl H2Client {
    pub(super) fn new(client: SimpleClient) -> Self {
        let created = now();
        Self {
            client,
            state: Rc::new(H2State {
                created: Cell::new(created),
                used: Cell::new(created),
                guards: Cell::new(0),
            }),
            activity: None,
            reservation: None,
        }
    }

    /// Starts a new request on this connection.
    ///
    /// Returns `None` if a stream cannot be reserved.
    pub(super) fn begin(&self) -> Option<Self> {
        let reservation = self.client.reserve()?;
        Some(Self {
            client: self.client.clone(),
            state: self.state.clone(),
            activity: Some(H2Activity::new(&self.state)),
            reservation: Some(reservation),
        })
    }

    #[cfg(test)]
    pub(super) fn set_times(&self, created: Instant, used: Instant) {
        self.state.created.set(created);
        self.state.used.set(used);
    }

    /// Returns when the connection was created.
    pub(super) fn created(&self) -> Instant {
        self.state.created.get()
    }

    /// Returns when the last request completed.
    pub(super) fn used(&self) -> Instant {
        self.state.used.get()
    }

    /// Returns whether the connection has no in-flight requests.
    pub(super) fn is_idle(&self) -> bool {
        self.state.guards.get() == 0
    }

    /// Returns the number of open streams, including reserved streams.
    pub(super) fn streams(&self) -> u32 {
        self.client.active_streams()
    }

    /// Returns whether the connection can start another request.
    ///
    /// A zero `max_streams` uses only the peer's stream limit.
    pub(super) fn has_capacity(&self, max_streams: u32) -> bool {
        let limit = match (self.client.max_streams(), max_streams) {
            (Some(peer), 0) => Some(peer),
            (Some(peer), max) => Some(peer.min(max)),
            (None, 0) => None,
            (None, max) => Some(max),
        };
        limit.is_none_or(|limit| self.streams() < limit)
    }

    pub(super) fn is_disconnecting(&self) -> bool {
        self.client.is_disconnecting()
    }

    pub(super) fn tag(&self) -> &'static str {
        self.client.tag()
    }

    pub(super) fn service(&self) -> &'static str {
        self.client.service()
    }

    pub(super) fn close(&self) {
        self.client.close();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Message, StatusCode};
    use crate::io::{Io, IoBoxed, testing::IoTest};
    use crate::{SharedCfg, time::sleep};

    #[test]
    fn test_connection_headers_are_removed() {
        let mut head = Message::<crate::http::RequestHead>::new();
        for (name, value) in [
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("keep-alive", "timeout=5"),
            ("proxy-connection", "keep-alive"),
            ("host", "example.com"),
            ("te", "gzip"),
            ("x-head", "1"),
        ] {
            head.headers.append(
                header::HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        let mut extra = HeaderMap::new();
        extra.insert(header::UPGRADE, HeaderValue::from_static("h2c"));
        extra.insert(header::TE, HeaderValue::from_static("Trailers"));
        extra.insert(
            header::HeaderName::from_static("x-extra"),
            HeaderValue::from_static("2"),
        );
        let req = ClientRawRequest {
            head,
            headers: Some(extra),
            size: BodySize::None,
        };

        let hdrs = h2_headers(&req);
        let mut names: Vec<_> = hdrs.keys().map(header::HeaderName::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["te", "x-extra", "x-head"]);
        assert_eq!(hdrs.get(header::TE).unwrap().as_bytes(), b"trailers");
    }

    #[crate::rt_test]
    async fn test_skip_interim_responses() {
        let (io, server) = IoTest::create();
        io.remote_buffer_cap(64 * 1024);
        let client = H2Client::new(SimpleClient::new(
            IoBoxed::from(Io::new(io, SharedCfg::default())),
            false,
            ByteString::from_static("localhost"),
        ));
        server.write([0, 0, 0, 4, 0, 0, 0, 0, 0]);
        sleep(Millis(50)).await;

        let req = ClientRawRequest {
            head: Message::new(),
            headers: None,
            size: BodySize::None,
        };
        let fut = crate::rt::spawn(send_request_inner(client, req, Body::None, Millis(5_000)));
        sleep(Millis(50)).await;

        // 103 and 100 interim responses, then 200 with END_STREAM
        server.write([0, 0, 5, 1, 4, 0, 0, 0, 1, 0x08, 3, b'1', b'0', b'3']);
        server.write([0, 0, 5, 1, 4, 0, 0, 0, 1, 0x08, 3, b'1', b'0', b'0']);
        server.write([0, 0, 1, 1, 5, 0, 0, 0, 1, 0x88]);

        let (head, payload) = fut.await.unwrap().unwrap();
        assert_eq!(head.status, StatusCode::OK);
        assert!(matches!(payload, Payload::None));
    }
}
