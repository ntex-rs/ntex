use std::{cell::Cell, fmt::Write, future::poll_fn, io, rc::Rc, time::Instant};

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

    // merging headers from head and extra headers.
    let empty = HeaderMap::new();
    let extra_headers = req.headers.as_ref().unwrap_or(&empty);
    let mut hdrs: HeaderMap = req
        .head
        .headers
        .iter()
        .filter(|(name, _)| {
            // h2 does not user connection headers
            !(matches!(*name, &header::CONNECTION | &header::TRANSFER_ENCODING)
                || extra_headers.contains_key(*name))
        })
        .chain(extra_headers.iter())
        .collect();

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
    let path = uri.path_and_query().map_or_else(
        || ByteString::from(uri.path()),
        |p| {
            let mut buf = BytesMut::new();
            write!(&mut buf, "{p}").unwrap();
            ByteString::try_from(buf).unwrap()
        },
    );
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
                log::error!("{}: Cannot send body: {e:?}", snd_stream.tag());
                snd_stream.reset(frame::Reason::INTERNAL_ERROR);
            }
        });
    }

    timeout_checked(timeout, get_response(rcv_stream, client.activity.clone()))
        .await
        .map_err(|()| Error::from(ClientError::Timeout))
        .and_then(|res| res)
}

async fn get_response(
    rcv_stream: RecvStream,
    activity: Option<H2Activity>,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    let h2::Message { stream, kind } = rcv_stream
        .recv()
        .await
        .ok_or(ClientError::Connect(ConnectError::Disconnected(None)))?;

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
                                        pl.feed_eof(Bytes::new());
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
                                            h2::StreamEof::Data(data) => {
                                                pl.feed_eof(data);
                                            }
                                            h2::StreamEof::Trailers(_) => {
                                                pl.feed_eof(Bytes::new());
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
                None => Err(Error::from(ClientError::H2(
                    h2::OperationError::Connection(h2::ConnectionError::MissingPseudo("Status")),
                ))),
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
    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
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
