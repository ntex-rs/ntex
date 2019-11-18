use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_utils::mpsc;
use futures::Stream;

use crate::dispatcher::FramedMessage;
use crate::sink::Sink;

pub struct Connect<Io, St = (), Codec = ()> {
    io: Io,
    _t: PhantomData<(St, Codec)>,
}

impl<Io> Connect<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(io: Io) -> Self {
        Self {
            io,
            _t: PhantomData,
        }
    }

    pub fn codec<Codec>(self, codec: Codec) -> ConnectResult<Io, (), Codec>
    where
        Codec: Encoder + Decoder + Unpin,
    {
        let (tx, rx) = mpsc::channel();
        let sink = Sink::new(tx);

        ConnectResult {
            state: (),
            framed: Framed::new(self.io, codec),
            rx,
            sink,
        }
    }
}

pub struct ConnectResult<Io, St, Codec: Encoder + Decoder> {
    pub(crate) state: St,
    pub(crate) framed: Framed<Io, Codec>,
    pub(crate) rx: mpsc::Receiver<FramedMessage<<Codec as Encoder>::Item>>,
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
            rx: self.rx,
            sink: self.sink,
        }
    }
}

impl<Io, St, Codec> Unpin for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Decoder + Unpin,
{
}

impl<Io, St, Codec> Stream for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Decoder + Unpin,
{
    type Item = Result<<Codec as Decoder>::Item, <Codec as Decoder>::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.get_mut().framed.next_item(cx)
    }
}

impl<Io, St, Codec> futures::Sink<<Codec as Encoder>::Item> for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite + Unpin,
    Codec: Encoder + Decoder + Unpin,
{
    type Error = <Codec as Encoder>::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().framed.is_ready(cx)
    }

    fn start_send(
        self: Pin<&mut Self>,
        item: <Codec as Encoder>::Item,
    ) -> Result<(), Self::Error> {
        self.get_mut().framed.write(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().framed.flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().framed.close(cx)
    }
}
