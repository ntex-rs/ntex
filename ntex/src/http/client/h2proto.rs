use std::{future::poll_fn, io};

use ntex_h2::client::{RecvStream, SimpleClient};
use ntex_h2::{self as h2, frame};

use crate::http::body::{BodySize, MessageBody};
use crate::http::header::{self, HeaderMap, HeaderValue};
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::{h2::payload, payload::Payload, Method, Version};
use crate::time::{timeout_checked, Millis};
use crate::util::{ByteString, Bytes};

use super::error::{ConnectError, SendRequestError};

pub(super) async fn send_request<B>(
    client: H2Client,
    head: RequestHeadType,
    body: B,
    timeout: Millis,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    B: MessageBody,
{
    log::trace!("Sending client request: {:?} {:?}", head, body.size());
    let length = body.size();
    let eof = if head.as_ref().method == Method::HEAD {
        true
    } else {
        matches!(
            length,
            BodySize::None | BodySize::Empty | BodySize::Sized(0)
        )
    };

    // Extracting extra headers from RequestHeadType. HeaderMap::new() does not allocate.
    let (head, extra_headers) = match head {
        RequestHeadType::Owned(head) => (RequestHeadType::Owned(head), HeaderMap::new()),
        RequestHeadType::Rc(head, extra_headers) => (
            RequestHeadType::Rc(head, None),
            extra_headers.unwrap_or_else(HeaderMap::new),
        ),
    };

    // merging headers from head and extra headers.
    let mut hdrs: HeaderMap = head
        .as_ref()
        .headers
        .iter()
        .filter(|(name, _)| {
            !matches!(*name, &header::CONNECTION | &header::TRANSFER_ENCODING)
                || !extra_headers.contains_key(*name)
        })
        .chain(extra_headers.iter())
        .collect();

    // Content length
    match length {
        BodySize::None | BodySize::Stream => (),
        BodySize::Empty => {
            hdrs.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"))
        }
        BodySize::Sized(len) => hdrs.insert(
            header::CONTENT_LENGTH,
            HeaderValue::try_from(format!("{}", len)).unwrap(),
        ),
    };

    // send request
    let uri = &head.as_ref().uri;
    let path = uri
        .path_and_query()
        .map(|p| ByteString::from(format!("{}", p)))
        .unwrap_or_else(|| ByteString::from(uri.path()));
    let (snd_stream, rcv_stream) = client
        .client
        .send(head.as_ref().method.clone(), path, hdrs, eof)
        .await?;

    // send body
    if !eof {
        // sending body is async process, we can handle upload and download
        // at the same time
        let _ = crate::rt::spawn(async move {
            if let Err(e) = send_body(body, &snd_stream).await {
                log::error!("Cannot send body: {:?}", e);
                snd_stream.reset(frame::Reason::INTERNAL_ERROR);
            }
        });
    }

    timeout_checked(timeout, get_response(rcv_stream))
        .await
        .map_err(|_| SendRequestError::Timeout)
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
                "{:?} got response (eof: {}): {:#?}\nheaders: {:#?}",
                stream.id(),
                eof,
                pseudo,
                headers
            );

            match pseudo.status {
                Some(status) => {
                    let mut head = ResponseHead::new(status);
                    head.headers = headers;
                    head.version = Version::HTTP_2;

                    let payload = if !eof {
                        log::debug!("Creating local payload stream for {:?}", stream.id());
                        let (mut pl, payload) =
                            payload::Payload::create(stream.empty_capacity());
                        let _ = crate::rt::spawn(async move {
                            loop {
                                let h2::Message { stream, kind } =
                                    match rcv_stream.recv().await {
                                        Some(msg) => msg,
                                        None => {
                                            pl.feed_eof(Bytes::new());
                                            break;
                                        }
                                    };
                                match kind {
                                    h2::MessageKind::Data(data, cap) => {
                                        log::debug!(
                                            "Got data chunk for {:?}: {:?}",
                                            stream.id(),
                                            data.len()
                                        );
                                        pl.feed_data(data, cap);
                                    }
                                    h2::MessageKind::Eof(item) => {
                                        log::debug!(
                                            "Got payload eof for {:?}: {:?}",
                                            stream.id(),
                                            item
                                        );
                                        match item {
                                            h2::StreamEof::Data(data) => {
                                                pl.feed_eof(data);
                                            }
                                            h2::StreamEof::Trailers(_) => {
                                                pl.feed_eof(Bytes::new());
                                            }
                                            h2::StreamEof::Error(err) => {
                                                pl.set_error(err.into())
                                            }
                                        }
                                    }
                                    h2::MessageKind::Disconnect(err) => {
                                        log::debug!("Connection is disconnected {:?}", err);
                                        pl.set_error(
                                            io::Error::new(io::ErrorKind::Other, err)
                                                .into(),
                                        );
                                    }
                                    _ => {
                                        pl.set_error(
                                            io::Error::new(
                                                io::ErrorKind::Other,
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
                    } else {
                        Payload::None
                    };
                    Ok((head, payload))
                }
                None => Err(SendRequestError::H2(h2::OperationError::Connection(
                    h2::ConnectionError::MissingPseudo("Status"),
                ))),
            }
        }
        _ => Err(SendRequestError::Error(Box::new(io::Error::new(
            io::ErrorKind::Other,
            "unexpected h2 message",
        )))),
    }
}

async fn send_body<B: MessageBody>(
    mut body: B,
    stream: &h2::client::SendStream,
) -> Result<(), SendRequestError> {
    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
            Some(Ok(b)) => {
                log::debug!("{:?} sending chunk, {} bytes", stream.id(), b.len());
                stream.send_payload(b, false).await?
            }
            Some(Err(e)) => return Err(e.into()),
            None => {
                log::debug!("{:?} eof of send stream ", stream.id());
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

    pub(super) fn close(&self) {
        self.client.close()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}
