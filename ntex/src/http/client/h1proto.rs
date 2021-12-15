use std::{io, io::Write, pin::Pin, task::Context, task::Poll, time::Instant};

use crate::http::body::{BodySize, MessageBody};
use crate::http::error::PayloadError;
use crate::http::h1;
use crate::http::header::{HeaderMap, HeaderValue, HOST};
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::{Payload, PayloadStream};
use crate::io::IoBoxed;
use crate::util::{next, poll_fn, send, BufMut, Bytes, BytesMut};
use crate::{Sink, Stream};

use super::connection::{Connection, ConnectionType};
use super::error::{ConnectError, SendRequestError};
use super::pool::Acquired;

pub(super) async fn send_request<B>(
    io: IoBoxed,
    mut head: RequestHeadType,
    body: B,
    created: Instant,
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

            match HeaderValue::from_maybe_shared(wrt.get_mut().split().freeze()) {
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

    // let io = H1Connection {
    //     created,
    //     pool,
    //     io: Some(io),
    // };

    // send request
    let codec = h1::ClientCodec::default();
    io.send((head, body.size()).into(), &codec).await?;

    // send request body
    match body.size() {
        BodySize::None | BodySize::Empty | BodySize::Sized(0) => (),
        _ => {
            send_body(body, &io, &codec).await?;
        }
    };

    // read response and init read body
    let head = if let Some(result) = io.next(&codec).await? {
        result
    } else {
        return Err(SendRequestError::from(ConnectError::Disconnected));
    };

    match codec.message_type() {
        h1::MessageType::None => {
            let force_close = !codec.keepalive();
            release_connection(io, force_close, created, pool);
            Ok((head, Payload::None))
        }
        _ => {
            let pl: PayloadStream = Box::pin(PlStream::new(io, codec, created, pool));
            Ok((head, pl.into()))
        }
    }
}

pub(super) async fn open_tunnel(
    io: IoBoxed,
    head: RequestHeadType,
) -> Result<(ResponseHead, IoBoxed, h1::ClientCodec), SendRequestError> {
    // create Framed and send request
    let codec = h1::ClientCodec::default();
    io.send((head, BodySize::None).into(), &codec).await?;

    // read response
    if let Some(head) = io.next(&codec).await? {
        Ok((head, io, codec))
    } else {
        Err(SendRequestError::from(ConnectError::Disconnected))
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
    let wrt = io.write();

    loop {
        match poll_fn(|cx| body.poll_next_chunk(cx)).await {
            Some(result) => {
                if !wrt.encode(h1::Message::Chunk(Some(result?)), codec)? {
                    wrt.flush(false).await?;
                }
            }
            None => {
                wrt.encode(h1::Message::Chunk(None), codec)?;
                break;
            }
        }
    }
    wrt.flush(true).await?;

    Ok(())
}

pub(super) struct PlStream {
    io: Option<IoBoxed>,
    codec: h1::ClientPayloadCodec,
    created: Instant,
    pool: Option<Acquired>,
}

impl PlStream {
    fn new(
        io: IoBoxed,
        codec: h1::ClientCodec,
        created: Instant,
        pool: Option<Acquired>,
    ) -> Self {
        PlStream {
            io: Some(io),
            codec: codec.into_payload_codec(),
            created,
            pool,
        }
    }
}

impl Stream for PlStream {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut();

        match this.io.as_ref().unwrap().poll_next(&this.codec, cx)? {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(chunk)) => {
                if let Some(chunk) = chunk {
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    let io = this.io.take().unwrap();
                    let force_close = !this.codec.keepalive();
                    release_connection(io, force_close, this.created, this.pool.take());
                    Poll::Ready(None)
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

fn release_connection(
    io: IoBoxed,
    force_close: bool,
    created: Instant,
    mut pool: Option<Acquired>,
) {
    if force_close || io.is_closed() || io.read().with_buf(|buf| !buf.is_empty()) {
        if let Some(mut pool) = pool.take() {
            pool.close(Connection::new(ConnectionType::H1(io), created, None));
        }
    } else if let Some(mut pool) = pool.take() {
        pool.release(Connection::new(ConnectionType::H1(io), created, None));
    }
}
