use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;

use crate::channel::mpsc::Receiver;
use crate::codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};

pub struct Handshake<Io, Codec>
where
    Codec: Encoder + Decoder,
{
    io: Io,
    _t: PhantomData<Codec>,
}

impl<Io, Codec> Handshake<Io, Codec>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Decoder,
{
    pub(crate) fn new(io: Io) -> Self {
        Self {
            io,
            _t: PhantomData,
        }
    }

    pub fn codec(
        self,
        codec: Codec,
    ) -> HandshakeResult<Io, (), Codec, Receiver<<Codec as Encoder>::Item>> {
        HandshakeResult {
            state: (),
            out: None,
            framed: Framed::new(self.io, codec),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct HandshakeResult<Io, St, Codec, Out> {
        pub(crate) state: St,
        pub(crate) out: Option<Out>,
        pub(crate) framed: Framed<Io, Codec>,
    }
}

impl<Io, St, Codec: Encoder + Decoder, Out: Unpin> HandshakeResult<Io, St, Codec, Out> {
    #[inline]
    pub fn io(&mut self) -> &mut Framed<Io, Codec> {
        &mut self.framed
    }

    pub fn out<U>(self, out: U) -> HandshakeResult<Io, St, Codec, U>
    where
        U: Stream<Item = <Codec as Encoder>::Item> + Unpin,
    {
        HandshakeResult {
            state: self.state,
            framed: self.framed,
            out: Some(out),
        }
    }

    #[inline]
    pub fn state<S>(self, state: S) -> HandshakeResult<Io, S, Codec, Out> {
        HandshakeResult {
            state,
            framed: self.framed,
            out: self.out,
        }
    }
}

impl<Io, St, Codec, Out> Stream for HandshakeResult<Io, St, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Decoder,
{
    type Item = Result<<Codec as Decoder>::Item, <Codec as Decoder>::Error>;

    #[inline]
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.framed.next_item(cx)
    }
}

impl<Io, St, Codec, Out> futures::Sink<Codec::Item>
    for HandshakeResult<Io, St, Codec, Out>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Unpin,
    Codec::Error: From<std::io::Error>,
{
    type Error = Codec::Error;

    #[inline]
    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.framed).poll_ready(cx)
    }

    #[inline]
    fn start_send(
        mut self: Pin<&mut Self>,
        item: <Codec as Encoder>::Item,
    ) -> Result<(), Self::Error> {
        Pin::new(&mut self.framed).start_send(item)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.framed).poll_flush(cx)
    }

    #[inline]
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.framed).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::lazy;
    use futures::{Sink, StreamExt};
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::testing::Io;

    #[allow(clippy::declare_interior_mutable_const)]
    const BLOB: Bytes = Bytes::from_static(b"GET /test HTTP/1.1\r\n\r\n");

    #[ntex_rt::test]
    async fn test_result() {
        let (client, server) = Io::create();
        client.remote_buffer_cap(1024);
        let server = Framed::new(server, BytesCodec);

        let mut hnd = HandshakeResult {
            state: (),
            out: Some(()),
            framed: server,
        };

        client.write(BLOB);
        let item = hnd.next().await.unwrap().unwrap();
        assert_eq!(item, BLOB);

        assert!(lazy(|cx| Pin::new(&mut hnd).poll_ready(cx))
            .await
            .is_ready());

        Pin::new(&mut hnd).start_send(BLOB).unwrap();
        assert_eq!(client.read_any(), b"".as_ref());
        assert_eq!(hnd.io().read_buf(), b"".as_ref());
        assert_eq!(hnd.io().write_buf(), &BLOB[..]);

        assert!(lazy(|cx| Pin::new(&mut hnd).poll_flush(cx))
            .await
            .is_ready());
        assert_eq!(client.read_any(), &BLOB[..]);

        assert!(lazy(|cx| Pin::new(&mut hnd).poll_close(cx))
            .await
            .is_pending());
        client.close().await;
        assert!(lazy(|cx| Pin::new(&mut hnd).poll_close(cx))
            .await
            .is_ready());
        assert!(client.is_closed());
    }
}
