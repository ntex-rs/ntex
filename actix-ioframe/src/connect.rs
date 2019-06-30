use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use futures::unsync::mpsc;

use crate::cell::Cell;
use crate::dispatcher::FramedMessage;
use crate::sink::Sink;

pub struct Connect<Io, St = (), Codec = ()> {
    io: IoItem<Io, Codec>,
    state: St,
}

enum IoItem<Io, Codec> {
    Io(Io),
    Framed(Framed<Io, Codec>),
}

impl<Io, C> IoItem<Io, C>
where
    Io: AsyncRead + AsyncWrite,
{
    fn into_codec<Codec>(self, codec: Codec) -> IoItem<Io, Codec>
    where
        Codec: Encoder + Decoder,
    {
        match self {
            IoItem::Io(io) => IoItem::Framed(Framed::new(io, codec)),
            IoItem::Framed(framed) => IoItem::Framed(framed.into_framed(codec)),
        }
    }

    fn as_framed(&mut self) -> &mut Framed<Io, C>
    where
        C: Encoder + Decoder,
    {
        match self {
            IoItem::Io(_) => panic!("Codec is not set"),
            IoItem::Framed(ref mut framed) => framed,
        }
    }

    fn into_framed(self) -> Framed<Io, C>
    where
        C: Encoder + Decoder,
    {
        match self {
            IoItem::Io(_) => panic!("Codec is not set"),
            IoItem::Framed(framed) => framed,
        }
    }
}

impl<Io> Connect<Io> {
    pub(crate) fn new(io: Io) -> Self {
        Self {
            io: IoItem::Io(io),
            state: (),
        }
    }
}

impl<Io, S, C> Connect<Io, S, C>
where
    Io: AsyncRead + AsyncWrite,
{
    pub fn codec<Codec>(self, codec: Codec) -> Connect<Io, S, Codec>
    where
        Codec: Encoder + Decoder,
    {
        Connect {
            io: self.io.into_codec(codec),
            state: self.state,
        }
    }

    pub fn state<St>(self, state: St) -> Connect<Io, St, C> {
        Connect { state, io: self.io }
    }
}

impl<Io, S, C> Connect<Io, S, C>
where
    C: Encoder + Decoder,
    Io: AsyncRead + AsyncWrite,
{
    pub fn into_result(self) -> ConnectResult<Io, S, C> {
        let (tx, rx) = mpsc::unbounded();
        let sink = Sink::new(tx);

        ConnectResult {
            state: Cell::new(self.state),
            framed: self.io.into_framed(),
            rx,
            sink,
        }
    }
}

impl<Io, St, Codec> futures::Stream for Connect<Io, St, Codec>
where
    Codec: Encoder + Decoder,
    Io: AsyncRead + AsyncWrite,
{
    type Item = <Codec as Decoder>::Item;
    type Error = <Codec as Decoder>::Error;

    fn poll(&mut self) -> futures::Poll<Option<Self::Item>, Self::Error> {
        self.io.as_framed().poll()
    }
}

impl<Io, St, Codec> futures::Sink for Connect<Io, St, Codec>
where
    Codec: Encoder + Decoder,
    Io: AsyncRead + AsyncWrite,
{
    type SinkItem = <Codec as Encoder>::Item;
    type SinkError = <Codec as Encoder>::Error;

    fn start_send(
        &mut self,
        item: Self::SinkItem,
    ) -> futures::StartSend<Self::SinkItem, Self::SinkError> {
        self.io.as_framed().start_send(item)
    }

    fn poll_complete(&mut self) -> futures::Poll<(), Self::SinkError> {
        self.io.as_framed().poll_complete()
    }

    fn close(&mut self) -> futures::Poll<(), Self::SinkError> {
        self.io.as_framed().close()
    }
}

pub struct ConnectResult<Io, St, Codec: Encoder + Decoder> {
    pub(crate) state: Cell<St>,
    pub(crate) framed: Framed<Io, Codec>,
    pub(crate) rx: mpsc::UnboundedReceiver<FramedMessage<<Codec as Encoder>::Item>>,
    pub(crate) sink: Sink<<Codec as Encoder>::Item>,
}

impl<Io, St, Codec: Encoder + Decoder> ConnectResult<Io, St, Codec> {
    #[inline]
    pub fn sink(&self) -> &Sink<<Codec as Encoder>::Item> {
        &self.sink
    }
}

impl<Io, St, Codec> futures::Stream for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
{
    type Item = <Codec as Decoder>::Item;
    type Error = <Codec as Decoder>::Error;

    fn poll(&mut self) -> futures::Poll<Option<Self::Item>, Self::Error> {
        self.framed.poll()
    }
}

impl<Io, St, Codec> futures::Sink for ConnectResult<Io, St, Codec>
where
    Io: AsyncRead + AsyncWrite,
    Codec: Encoder + Decoder,
{
    type SinkItem = <Codec as Encoder>::Item;
    type SinkError = <Codec as Encoder>::Error;

    fn start_send(
        &mut self,
        item: Self::SinkItem,
    ) -> futures::StartSend<Self::SinkItem, Self::SinkError> {
        self.framed.start_send(item)
    }

    fn poll_complete(&mut self) -> futures::Poll<(), Self::SinkError> {
        self.framed.poll_complete()
    }

    fn close(&mut self) -> futures::Poll<(), Self::SinkError> {
        self.framed.close()
    }
}
