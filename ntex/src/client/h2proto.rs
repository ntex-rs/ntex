use std::{future::poll_fn, io, rc::Rc};

use ntex_h2::{self as h2, client::RecvStream, client::SimpleClient, frame};

use crate::http::ResponseHead;
use crate::http::body::{Body, BodySize, MessageBody};
use crate::http::header::{self, HeaderMap, HeaderValue};
use crate::http::{Method, Payload, Version, h2::Payload as H2Payload};
use crate::time::{Millis, timeout_checked};
use crate::util::{ByteString, Bytes, Either, select};

use super::ClientRawRequest;
use super::error::{ConnectError, SendRequestError};

pub(super) async fn send_request(
    client: H2Client,
    req: ClientRawRequest,
    body: Body,
    timeout: Millis,
) -> Result<(ResponseHead, Payload), SendRequestError> {
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
        BodySize::Sized(len) => hdrs.insert(
            header::CONTENT_LENGTH,
            HeaderValue::try_from(format!("{len}")).unwrap(),
        ),
    }

    // send request
    let uri = &req.head.uri;
    let path = uri.path_and_query().map_or_else(
        || ByteString::from(uri.path()),
        |p| ByteString::from(format!("{p}")),
    );
    let (snd_stream, rcv_stream) = client
        .client
        .send(req.head.method.clone(), path, hdrs, eof)
        .await?;

    // send body
    if !eof {
        // sending body is async process, we can handle upload and download
        // at the same time
        crate::rt::spawn(async move {
            if let Err(e) = send_body(body, &snd_stream).await {
                log::error!("{}: Cannot send body: {e:?}", snd_stream.tag());
                snd_stream.reset(frame::Reason::INTERNAL_ERROR);
            }
        });
    }

    timeout_checked(timeout, get_response(rcv_stream))
        .await
        .map_err(|()| SendRequestError::Timeout)
        .and_then(|res| res)
}

async fn get_response(
    rcv_stream: RecvStream,
) -> Result<(ResponseHead, Payload), SendRequestError> {
    let h2::Message { stream, kind } = rcv_stream
        .recv()
        .await
        .ok_or(SendRequestError::Connect(ConnectError::Disconnected(None)))?;
    match kind {
        h2::MessageKind::Headers {
            pseudo,
            headers,
            eof,
        } => {
            log::trace!(
                "{}: {:?} got response (eof: {eof}): {pseudo:#?}\nheaders: {headers:#?}",
                stream.tag(),
                stream.id(),
            );

            match pseudo.status {
                Some(status) => {
                    let mut head = ResponseHead::new(status);
                    head.headers = headers;
                    head.version = Version::HTTP_2;

                    let payload = if eof {
                        Payload::None
                    } else {
                        log::debug!(
                            "{}: Creating local payload stream for {:?}",
                            stream.tag(),
                            stream.id()
                        );
                        let (pl, payload) = H2Payload::create(stream.empty_capacity());

                        crate::rt::spawn(async move {
                            loop {
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
                                        log::trace!(
                                            "{}: Got data chunk for {:?}: {:?}",
                                            stream.tag(),
                                            stream.id(),
                                            data.len()
                                        );
                                        pl.feed_data(data, cap);
                                    }
                                    h2::MessageKind::Eof(item) => {
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
                                                pl.set_error(err.into());
                                            }
                                        }
                                    }
                                    h2::MessageKind::Disconnect(err) => {
                                        log::trace!(
                                            "{}: Connection is disconnected {err:?}",
                                            stream.tag(),
                                        );
                                        pl.set_error(
                                            io::Error::new(
                                                io::ErrorKind::UnexpectedEof,
                                                err,
                                            )
                                            .into(),
                                        );
                                    }
                                    h2::MessageKind::Headers { .. } => {
                                        pl.set_error(
                                            io::Error::new(
                                                io::ErrorKind::Unsupported,
                                                "unexpected h2 message",
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
                None => Err(SendRequestError::H2(h2::OperationError::Connection(
                    h2::ConnectionError::MissingPseudo("Status"),
                ))),
            }
        }
        _ => Err(SendRequestError::Error(Rc::new(io::Error::new(
            io::ErrorKind::Unsupported,
            "unexpected h2 message",
        )))),
    }
}

async fn send_body(
    mut body: Body,
    stream: &h2::client::SendStream,
) -> Result<(), SendRequestError> {
    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
            Some(Ok(b)) => {
                log::trace!(
                    "{}: {:?} sending chunk, {} bytes",
                    stream.tag(),
                    stream.id(),
                    b.len()
                );
                stream.send_payload(b, false).await?;
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                log::trace!("{}: {:?} eof of send stream ", stream.tag(), stream.id());
                stream.send_payload(Bytes::new(), true).await?;
                return Ok(());
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct H2Client {
    client: SimpleClient,
}

impl H2Client {
    pub(super) fn new(client: SimpleClient) -> Self {
        Self { client }
    }

    pub(super) fn tag(&self) -> &'static str {
        self.client.tag()
    }

    pub(super) fn close(&self) {
        self.client.close();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}
