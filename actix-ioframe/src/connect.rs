use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use futures::Stream;

use crate::sink::Sink;

pub struct Connect<Io, Codec, St = ()>
where
    Codec: Encoder + Decoder,
{
    io: Io,
    sink: Sink<<Codec as Encoder>::Item>,
    _t: PhantomData<(St, Codec)>,
}

impl<Io, Codec> Connect<Io, Codec>
where
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
{
    pub(crate) fn new(io: Io, sink: Sink<<Codec as Encoder>::Item>) -> Self {
        Self {
            io,
            sink,
            _t: PhantomData,
        }
    }

    pub fn codec(self, codec: Codec) -> ConnectResult<Io, (), Codec> {
        ConnectResult {
            state: (),
            sink: self.sink,
            framed: Framed::new(self.io, codec),
        }
    }
}

#[pin_project::pin_project]
pub struct ConnectResult<Io, St, Codec: Encoder + Decoder> {
    pub(crate) state: St,
    pub(crate) framed: Framed<Io, Codec>,
    pub(crate) sink: Sink<<Codec as Encoder>::Item>,
}

impl<Io, St, Codec: Encoder + Decoder> ConnectResult<Io, St, Codec> {
    #[inline]
    pub fn sink(&self) -> &Sink<<Codec as Encoder>::Item> {
        &self.sink
    }

    #[inline]
    pub fn get_ref(&self) -> &Io {
        self.framed.get_ref()
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut Io {
        self.framed.get_mut()
    }

    #[inline]
    pub fn state<S>(self, state: S) -> ConnectResult<Io, S, Codec> {
        ConnectResult {
            state,
            framed: self.framed,
            sink: self.sink,
        }
    }
}

impl<Io, St, Codec> Stream for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
{
    type Item = Result<<Codec as Decoder>::Item, <Codec as Decoder>::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().framed.next_item(cx)
    }
}

impl<Io, St, Codec> futures::Sink<<Codec as Encoder>::Item> for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
{
    type Error = <Codec as Encoder>::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.framed.is_write_ready() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn start_send(
        self: Pin<&mut Self>,
        item: <Codec as Encoder>::Item,
    ) -> Result<(), Self::Error> {
        self.project().framed.write(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().framed.flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().framed.close(cx)
    }
}
