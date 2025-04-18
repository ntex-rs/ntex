use std::task::{ready, Poll};
use std::{future::poll_fn, io, io::Write, pin::Pin, task, time::Instant};

use crate::http::body::{BodySize, MessageBody};
use crate::http::error::PayloadError;
use crate::http::header::{HeaderMap, HeaderValue, HOST};
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::{Payload, PayloadStream};
use crate::http::{h1, Version};
use crate::io::{IoBoxed, RecvError};
use crate::time::{timeout_checked, Millis};
use crate::util::{BufMut, Bytes, BytesMut, Stream};

use super::connection::{Connection, ConnectionType};
use super::error::{ConnectError, SendRequestError};
use super::pool::Acquired;

pub(super) async fn send_request<B>(
    io: IoBoxed,
    mut head: RequestHeadType,
    body: B,
    created: Instant,
    timeout: Millis,
    pool: Option<Acquired>,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    B: MessageBody,
{
    // set request host header
    if !head.as_ref().headers.contains_key(HOST)
        && !head.extra_headers().iter().any(|h| h.contains_key(HOST))
    {
        if let Some(host) = head.as_ref().uri.host() {
            let mut wrt = BytesMut::with_capacity(host.len() + 5).writer();

            let _ = match head.as_ref().uri.port_u16() {
                None | Some(80) | Some(443) => write!(wrt, "{}", host),
                Some(port) => write!(wrt, "{}:{}", host, port),
            };

            match HeaderValue::from_shared(wrt.get_mut().split()) {
                Ok(value) => match head {
                    RequestHeadType::Owned(ref mut head) => {
                        head.headers.insert(HOST, value)
                    }
                    RequestHeadType::Rc(_, ref mut extra_headers) => {
                        let headers = extra_headers.get_or_insert(HeaderMap::new());
                        headers.insert(HOST, value)
                    }
                },
                Err(e) => log::error!("Cannot set HOST header {}", e),
            }
        }
    }

    log::trace!(
        "sending http1 request {:?} body size: {:?}",
        head,
        body.size()
    );

    // send request
    let codec = h1::ClientCodec::default();
    io.send((head, body.size()).into(), &codec).await?;

    log::trace!("http1 request has been sent");

    // send request body
    match body.size() {
        BodySize::None | BodySize::Empty | BodySize::Sized(0) => (),
        _ => {
            send_body(body, &io, &codec).await?;
        }
    };

    log::trace!("reading http1 response");

    // read response and init read body
    let fut = async {
        if let Some(result) = io.recv(&codec).await? {
            log::trace!(
                "http1 response is received, type: {:?}, response: {:#?}",
                codec.message_type(),
                result
            );
            Ok(result)
        } else {
            Err(SendRequestError::from(ConnectError::Disconnected(None)))
        }
    };

    let head = timeout_checked(timeout, fut)
        .await
        .map_err(|_| SendRequestError::Timeout)
        .and_then(|res| res)?;

    match codec.message_type() {
        h1::MessageType::None => {
            release_connection(io, !codec.keepalive(), created, pool);
            Ok((head, Payload::None))
        }
        _ => {
            let pl: PayloadStream = Box::pin(PlStream::new(
                io,
                codec,
                created,
                pool,
                head.version == Version::HTTP_10,
            ));
            Ok((head, pl.into()))
        }
    }
}

/// send request body to the peer
pub(super) async fn send_body<B>(
    mut body: B,
    io: &IoBoxed,
    codec: &h1::ClientCodec,
) -> Result<(), SendRequestError>
where
    B: MessageBody,
{
    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
            Some(result) => {
                io.encode(h1::Message::Chunk(Some(result?)), codec)?;
                io.flush(false).await?;
            }
            None => {
                io.encode(h1::Message::Chunk(None), codec)?;
                break;
            }
        }
    }
    io.flush(true).await?;

    Ok(())
}

pub(super) struct PlStream {
    io: Option<IoBoxed>,
    codec: h1::ClientPayloadCodec,
    created: Instant,
    http_10: bool,
    pool: Option<Acquired>,
}

impl PlStream {
    fn new(
        io: IoBoxed,
        codec: h1::ClientCodec,
        created: Instant,
        pool: Option<Acquired>,
        http_10: bool,
    ) -> Self {
        PlStream {
            io: Some(io),
            codec: codec.into_payload_codec(),
            created,
            pool,
            http_10,
        }
    }
}

impl Stream for PlStream {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        loop {
            let item = ready!(this.io.as_ref().unwrap().poll_recv(&this.codec, cx));
            return Poll::Ready(Some(match item {
                Ok(chunk) => {
                    if let Some(chunk) = chunk {
                        Ok(chunk)
                    } else {
                        release_connection(
                            this.io.take().unwrap(),
                            !this.codec.keepalive(),
                            this.created,
                            this.pool.take(),
                        );
                        return Poll::Ready(None);
                    }
                }
                Err(RecvError::KeepAlive) => {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "Keep-alive").into())
                }
                Err(RecvError::Stop) => {
                    Err(io::Error::new(io::ErrorKind::Other, "Dispatcher stopped").into())
                }
                Err(RecvError::WriteBackpressure) => {
                    ready!(this.io.as_ref().unwrap().poll_flush(cx, false))?;
                    continue;
                }
                Err(RecvError::Decoder(err)) => Err(err),
                Err(RecvError::PeerGone(Some(err))) => {
                    Err(PayloadError::Incomplete(Some(err)))
                }
                Err(RecvError::PeerGone(None)) => {
                    if this.http_10 {
                        return Poll::Ready(None);
                    }
                    Err(PayloadError::Incomplete(None))
                }
            }));
        }
    }
}

fn release_connection(
    io: IoBoxed,
    force_close: bool,
    created: Instant,
    mut pool: Option<Acquired>,
) {
    if force_close || io.is_closed() || io.with_read_buf(|buf| !buf.is_empty()) {
        if let Some(mut pool) = pool.take() {
            pool.release(Connection::new(ConnectionType::H1(io), created, None), true);
        }
    } else if let Some(mut pool) = pool.take() {
        pool.release(
            Connection::new(ConnectionType::H1(io), created, None),
            false,
        );
    }
}
