use std::{
    cell::RefCell, fmt, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use bytes::{Bytes, BytesMut};
use ntex_codec::{Decoder, Encoder};

use super::{Codec, Frame, Message, ProtocolError};
use crate::{Sink, Stream};

/// Stream error
#[derive(Debug, Display)]
pub enum StreamError<E: fmt::Debug> {
    #[display(fmt = "StreamError::Stream({:?})", _0)]
    Stream(E),
    Protocol(ProtocolError),
}

impl<E: fmt::Debug> std::error::Error for StreamError<E> {}

impl<E: fmt::Debug> From<ProtocolError> for StreamError<E> {
    fn from(err: ProtocolError) -> Self {
        StreamError::Protocol(err)
    }
}

pin_project_lite::pin_project! {
/// Stream ws protocol decoder.
pub struct StreamDecoder<S, E> {
    #[pin]
    stream: S,
    codec: Codec,
    buf: BytesMut,
    _t: PhantomData<E>,
}
}

impl<S, E> StreamDecoder<S, E> {
    pub fn new(stream: S) -> Self {
        StreamDecoder::with(stream, Codec::new())
    }

    pub fn with(stream: S, codec: Codec) -> Self {
        StreamDecoder {
            stream,
            codec,
            buf: BytesMut::new(),
            _t: PhantomData,
        }
    }
}

impl<S, E> Stream for StreamDecoder<S, E>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: fmt::Debug,
{
    type Item = Result<Frame, StreamError<E>>;

    #[inline]
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut().project();

        loop {
            if !this.buf.is_empty() {
                match this.codec.decode(&mut this.buf) {
                    Ok(Some(item)) => return Poll::Ready(Some(Ok(item))),
                    Ok(None) => (),
                    Err(err) => return Poll::Ready(Some(Err(err.into()))),
                }
            }

            match this.stream.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(buf))) => {
                    this.buf.extend(&buf);
                    this = self.as_mut().project();
                }
                Poll::Ready(Some(Err(err))) => {
                    return Poll::Ready(Some(Err(StreamError::Stream(err))))
                }
                Poll::Ready(None) => return Poll::Ready(None),
            }
        }
    }
}

pin_project_lite::pin_project! {
/// Stream ws protocol decoder.
#[derive(Clone)]
pub struct StreamEncoder<S> {
    #[pin]
    sink: S,
    codec: Rc<RefCell<Codec>>,
}
}

impl<S> StreamEncoder<S> {
    pub fn new(sink: S) -> Self {
        StreamEncoder::with(sink, Codec::new())
    }

    pub fn with(sink: S, codec: Codec) -> Self {
        StreamEncoder {
            sink,
            codec: Rc::new(RefCell::new(codec)),
        }
    }
}

impl<S, E> Sink<Result<Message, E>> for StreamEncoder<S>
where
    S: Sink<Result<Bytes, E>>,
    S::Error: fmt::Debug,
{
    type Error = StreamError<S::Error>;

    #[inline]
    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.project()
            .sink
            .poll_ready(cx)
            .map_err(StreamError::Stream)
    }

    fn start_send(
        self: Pin<&mut Self>,
        item: Result<Message, E>,
    ) -> Result<(), Self::Error> {
        let this = self.project();

        match item {
            Ok(item) => {
                let mut buf = BytesMut::new();
                this.codec.borrow_mut().encode(item, &mut buf)?;
                this.sink
                    .start_send(Ok(buf.freeze()))
                    .map_err(StreamError::Stream)
            }
            Err(e) => this.sink.start_send(Err(e)).map_err(StreamError::Stream),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.project()
            .sink
            .poll_flush(cx)
            .map_err(StreamError::Stream)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        self.project()
            .sink
            .poll_close(cx)
            .map_err(StreamError::Stream)
    }
}

#[cfg(test)]
mod tests {
    use bytestring::ByteString;

    use super::*;
    use crate::{channel::mpsc, util::next, util::poll_fn, util::send};

    #[crate::rt_test]
    async fn test_decoder() {
        let (tx, rx) = mpsc::channel();
        let mut decoder = StreamDecoder::new(rx);

        let mut buf = BytesMut::new();
        let codec = Codec::new().client_mode();
        codec
            .encode(Message::Text(ByteString::from_static("test1")), &mut buf)
            .unwrap();
        codec
            .encode(Message::Text(ByteString::from_static("test2")), &mut buf)
            .unwrap();

        tx.send(Ok::<_, ()>(buf.split().freeze())).unwrap();
        let frame = next(&mut decoder).await.unwrap().unwrap();
        match frame {
            Frame::Text(data) => assert_eq!(data, b"test1"[..]),
            _ => panic!(),
        }
        let frame = next(&mut decoder).await.unwrap().unwrap();
        match frame {
            Frame::Text(data) => assert_eq!(data, b"test2"[..]),
            _ => panic!(),
        }
    }

    #[crate::rt_test]
    async fn test_encoder() {
        let (tx, mut rx) = mpsc::channel();
        let mut encoder = StreamEncoder::new(tx);

        send(
            &mut encoder,
            Ok::<_, ()>(Message::Text(ByteString::from_static("test"))),
        )
        .await
        .unwrap();
        poll_fn(|cx| Pin::new(&mut encoder).poll_flush(cx))
            .await
            .unwrap();
        poll_fn(|cx| Pin::new(&mut encoder).poll_close(cx))
            .await
            .unwrap();

        let data = next(&mut rx).await.unwrap().unwrap();
        assert_eq!(data, b"\x81\x04test".as_ref());
        assert!(next(&mut rx).await.is_none());
    }
}
