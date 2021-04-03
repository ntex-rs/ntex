use std::{io, io::Write, pin::Pin, task::Context, task::Poll, time};

use bytes::{BufMut, Bytes, BytesMut};

use crate::codec::{AsyncRead, AsyncWrite, Framed, ReadBuf};
use crate::http::body::{BodySize, MessageBody};
use crate::http::error::PayloadError;
use crate::http::h1;
use crate::http::header::{HeaderMap, HeaderValue, HOST};
use crate::http::message::{RequestHeadType, ResponseHead};
use crate::http::payload::{Payload, PayloadStream};
use crate::util::{next, poll_fn, send};
use crate::{Sink, Stream};

use super::connection::{ConnectionLifetime, ConnectionType, IoConnection};
use super::error::{ConnectError, SendRequestError};
use super::pool::Acquired;

pub(super) async fn send_request<T, B>(
    io: T,
    mut head: RequestHeadType,
    body: B,
    created: time::Instant,
    pool: Option<Acquired<T>>,
) -> Result<(ResponseHead, Payload), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
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

    let io = H1Connection {
        created,
        pool,
        io: Some(io),
    };

    // create Framed and send request
    let mut framed = Framed::new(io, h1::ClientCodec::default());
    send(&mut framed, (head, body.size()).into()).await?;

    // send request body
    match body.size() {
        BodySize::None | BodySize::Empty | BodySize::Sized(0) => (),
        _ => send_body(body, &mut framed).await?,
    };

    // read response and init read body
    let head = if let Some(result) = next(&mut framed).await {
        result.map_err(SendRequestError::from)?
    } else {
        return Err(SendRequestError::from(ConnectError::Disconnected));
    };

    match framed.get_codec().message_type() {
        h1::MessageType::None => {
            let force_close = !framed.get_codec().keepalive();
            release_connection(framed, force_close);
            Ok((head, Payload::None))
        }
        _ => {
            let pl: PayloadStream = Box::pin(PlStream::new(framed));
            Ok((head, pl.into()))
        }
    }
}

pub(super) async fn open_tunnel<T>(
    io: T,
    head: RequestHeadType,
) -> Result<(ResponseHead, Framed<T, h1::ClientCodec>), SendRequestError>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    // create Framed and send request
    let mut framed = Framed::new(io, h1::ClientCodec::default());
    send(&mut framed, (head, BodySize::None).into()).await?;

    // read response
    if let Some(result) = next(&mut framed).await {
        let head = result.map_err(SendRequestError::from)?;
        Ok((head, framed))
    } else {
        Err(SendRequestError::from(ConnectError::Disconnected))
    }
}

/// send request body to the peer
pub(super) async fn send_body<I, B>(
    mut body: B,
    framed: &mut Framed<I, h1::ClientCodec>,
) -> Result<(), SendRequestError>
where
    I: ConnectionLifetime,
    B: MessageBody,
{
    let mut eof = false;
    while !eof {
        while !eof && !framed.is_write_buf_full() {
            match poll_fn(|cx| body.poll_next_chunk(cx)).await {
                Some(result) => {
                    framed.write(h1::Message::Chunk(Some(result?)))?;
                }
                None => {
                    eof = true;
                    framed.write(h1::Message::Chunk(None))?;
                }
            }
        }

        if !framed.is_write_buf_empty() {
            poll_fn(|cx| match framed.flush(cx) {
                Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => {
                    if !framed.is_write_buf_full() {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Pending
                    }
                }
            })
            .await?;
        }
    }

    poll_fn(|cx| Pin::new(&mut *framed).poll_flush(cx)).await?;

    Ok(())
}

#[doc(hidden)]
/// HTTP client connection
pub(super) struct H1Connection<T> {
    io: Option<T>,
    created: time::Instant,
    pool: Option<Acquired<T>>,
}

impl<T> ConnectionLifetime for H1Connection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
{
    /// Close connection
    fn close(&mut self) {
        if let Some(mut pool) = self.pool.take() {
            if let Some(io) = self.io.take() {
                pool.close(IoConnection::new(
                    ConnectionType::H1(io),
                    self.created,
                    None,
                ));
            }
        }
    }

    /// Release this connection to the connection pool
    fn release(&mut self) {
        if let Some(mut pool) = self.pool.take() {
            if let Some(io) = self.io.take() {
                pool.release(IoConnection::new(
                    ConnectionType::H1(io),
                    self.created,
                    None,
                ));
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> AsyncRead for H1Connection<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io.as_mut().unwrap()).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + 'static> AsyncWrite for H1Connection<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io.as_mut().unwrap()).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(self.io.as_mut().unwrap()).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(self.io.as_mut().unwrap()).poll_shutdown(cx)
    }
}

pub(super) struct PlStream<Io> {
    framed: Option<Framed<Io, h1::ClientPayloadCodec>>,
}

impl<Io: ConnectionLifetime> PlStream<Io> {
    fn new(framed: Framed<Io, h1::ClientCodec>) -> Self {
        PlStream {
            framed: Some(framed.map_codec(|codec| codec.into_payload_codec())),
        }
    }
}

impl<Io: ConnectionLifetime> Stream for PlStream<Io> {
    type Item = Result<Bytes, PayloadError>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match this.framed.as_mut().unwrap().next_item(cx)? {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(chunk)) => {
                if let Some(chunk) = chunk {
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    let framed = this.framed.take().unwrap();
                    let force_close = !framed.get_codec().keepalive();
                    release_connection(framed, force_close);
                    Poll::Ready(None)
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

fn release_connection<T, U>(framed: Framed<T, U>, force_close: bool)
where
    T: ConnectionLifetime,
{
    let mut parts = framed.into_parts();
    if !force_close && parts.read_buf.is_empty() && parts.write_buf.is_empty() {
        parts.io.release()
    } else {
        parts.io.close()
    }
}
