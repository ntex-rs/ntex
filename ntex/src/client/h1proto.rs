#![allow(unused_imports)]
use std::task::{Poll, ready};
use std::{future::poll_fn, io, io::Write, pin::Pin, task, time::Instant};

use crate::error::{Error, ErrorMapping, with_service};
use crate::http::body::{Body, BodySize, MessageBody};
use crate::http::error::PayloadError;
use crate::http::header::{HOST, HeaderValue};
use crate::http::{Payload, PayloadStream, ResponseHead, Uri, h1};
use crate::io::{IoBoxed, RecvError};
use crate::service::cfg::Configuration;
use crate::time::{Millis, timeout_checked};
use crate::util::{BufMut, Bytes, BytesMut, Stream, lazy};

use super::connection::{Connection, ConnectionType};
use super::error::{ClientError, ConnectError};
use super::{ClientCodec, ClientPayloadCodec, ClientRawRequest, pool::Acquired};

pub(super) async fn send_request(
    io: IoBoxed,
    req: ClientRawRequest,
    body: Body,
    created: Instant,
    timeout: Millis,
    pool: Option<Acquired>,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    with_service(
        io.cfg().ctx().service(),
        send_request_inner(io, req, body, created, timeout, pool),
    )
    .await
}

async fn send_request_inner(
    io: IoBoxed,
    mut req: ClientRawRequest,
    body: Body,
    created: Instant,
    timeout: Millis,
    pool: Option<Acquired>,
) -> Result<(ResponseHead, Payload), Error<ClientError>> {
    // set request host header
    if !req.head.headers.contains_key(HOST)
        && let Some(value) = host_header(&req.head.uri)
    {
        req.head.headers.insert(HOST, value);
    }

    log::trace!(
        "{}: sending http1 request {req:?} body size: {:?}",
        io.tag(),
        body.size()
    );

    // send request
    let codec = ClientCodec::new(true, io.shared().get());
    io.send(req.into(), &codec).await.into_error()?;

    log::trace!("{}: http1 request has been sent", io.tag());

    // send request body
    match body.size() {
        BodySize::None | BodySize::Empty | BodySize::Sized(0) => (),
        _ => {
            if let Err(err) = send_body(body, &io, &codec).await {
                // the server may respond early, for example with `413`, and close
                // the connection before the body is sent, use the received response
                return if let Poll::Ready(Ok(head)) = lazy(|cx| io.poll_recv(&codec, cx)).await {
                    log::trace!(
                        "{}: http1 response is received before request body is sent",
                        io.tag()
                    );
                    codec.set_close();
                    Ok(response(io, codec, head, created, pool))
                } else {
                    Err(err)
                };
            }
        }
    }

    log::trace!("{}: reading http1 response", io.tag());

    // read response and init read body
    let fut = async {
        if let Some(result) = io.recv(&codec).await.into_error()? {
            log::trace!(
                "{}: http1 response is received, type: {:?}, response: {result:#?}",
                io.tag(),
                codec.message_type()
            );
            Ok(result)
        } else {
            Err(Error::from(ClientError::from(ConnectError::Disconnected(
                None,
            ))))
        }
    };

    let head = timeout_checked(timeout, fut)
        .await
        .map_err(|()| Error::from(ClientError::Timeout))
        .and_then(|res| res)?;

    Ok(response(io, codec, head, created, pool))
}

fn response(
    io: IoBoxed,
    codec: ClientCodec,
    head: ResponseHead,
    created: Instant,
    pool: Option<Acquired>,
) -> (ResponseHead, Payload) {
    if codec.message_type() == h1::MessageType::None {
        release_connection(io, !codec.keepalive(), created, pool);
        (head, Payload::None)
    } else {
        let pl: PayloadStream = Box::pin(PlStream::new(io, codec, created, pool));
        (head, pl.into())
    }
}

/// Builds the `Host` header value, the port is omitted if it is the scheme's default.
pub(crate) fn host_header(uri: &Uri) -> Option<HeaderValue> {
    let host = uri.host()?;
    let default_port = match uri.scheme_str() {
        Some("https" | "wss") => 443,
        _ => 80,
    };

    let mut wrt = BytesMut::with_capacity(host.len() + 6);
    let _ = match uri.port_u16() {
        Some(port) if port != default_port => write!(wrt, "{host}:{port}"),
        _ => write!(wrt, "{host}"),
    };

    match HeaderValue::from_shared(wrt.take()) {
        Ok(value) => Some(value),
        Err(e) => {
            log::error!("Cannot set HOST header {e}");
            None
        }
    }
}

/// send request body to the peer
pub(super) async fn send_body(
    mut body: Body,
    io: &IoBoxed,
    codec: &ClientCodec,
) -> Result<(), Error<ClientError>> {
    loop {
        if let Some(result) = poll_fn(|cx| body.poll_next_chunk(cx)).await {
            let chunk = result.into_error()?;
            #[cfg(feature = "trace")]
            let chunk_len = chunk.len();
            io.encode(h1::Message::Chunk(Some(chunk)), codec)
                .into_error()?;
            #[cfg(feature = "trace")]
            log::trace!(
                "{}: sending chunk, {} bytes, backpressure: {}",
                io.tag(),
                chunk_len,
                io.is_wr_backpressure()
            );
            if io.is_wr_backpressure() {
                io.flush(false).await.into_error()?;
                #[cfg(feature = "trace")]
                log::trace!("{}: flushed", io.tag());
            }
        } else {
            io.encode(h1::Message::Chunk(None), codec).into_error()?;
            io.flush(true).await.into_error()?;
            break;
        }
    }

    Ok(())
}

pub(super) struct PlStream {
    io: Option<IoBoxed>,
    codec: ClientPayloadCodec,
    created: Instant,
    eof_delimited: bool,
    pool: Option<Acquired>,
}

impl PlStream {
    fn new(io: IoBoxed, codec: ClientCodec, created: Instant, pool: Option<Acquired>) -> Self {
        let codec = codec.into_payload_codec();
        PlStream {
            io: Some(io),
            eof_delimited: codec.eof_delimited(),
            codec,
            created,
            pool,
        }
    }
}

impl Stream for PlStream {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();
        loop {
            let Some(io) = this.io.as_ref() else {
                return Poll::Ready(None);
            };
            let item = ready!(io.poll_recv(&this.codec, cx));
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
                Err(RecvError::KeepAlive) => Err(PayloadError::from(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Keep-alive",
                ))),
                Err(RecvError::WriteBackpressure) => {
                    ready!(this.io.as_ref().unwrap().poll_flush(cx, false))?;
                    continue;
                }
                Err(RecvError::Decoder(err)) => Err(err),
                Err(RecvError::PeerGone(Some(err))) => Err(PayloadError::Incomplete(Some(err))),
                Err(RecvError::PeerGone(None)) => {
                    if this.eof_delimited {
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
    if force_close || !io.is_active() || io.is_read_eof() || io.with_read_dst(|buf| !buf.is_empty())
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_header() {
        for (uri, host) in [
            ("http://example.com/", "example.com"),
            ("http://example.com:80/", "example.com"),
            ("http://example.com:443/", "example.com:443"),
            ("http://example.com:8080/", "example.com:8080"),
            ("https://example.com:443/", "example.com"),
            ("https://example.com:80/", "example.com:80"),
            ("ws://example.com:80/", "example.com"),
            ("wss://example.com:443/", "example.com"),
            ("wss://example.com:80/", "example.com:80"),
            ("http://[::1]:8080/", "[::1]:8080"),
        ] {
            let uri = Uri::try_from(uri).unwrap();
            assert_eq!(host_header(&uri).unwrap(), host, "{uri}");
        }
        assert!(host_header(&Uri::from_static("/path")).is_none());
    }
}
