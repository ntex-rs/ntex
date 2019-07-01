use std::marker::PhantomData;

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use futures::unsync::mpsc;

use crate::dispatcher::FramedMessage;
use crate::sink::Sink;

pub struct Connect<Io, St = (), Codec = ()> {
    io: Io,
    _t: PhantomData<(St, Codec)>,
}

impl<Io> Connect<Io>
where
    Io: AsyncRead + AsyncWrite,
{
    pub(crate) fn new(io: Io) -> Self {
        Self {
            io,
            _t: PhantomData,
        }
    }

    pub fn codec<Codec>(self, codec: Codec) -> ConnectResult<Io, (), Codec>
    where
        Codec: Encoder + Decoder,
    {
        let (tx, rx) = mpsc::unbounded();
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
    pub(crate) rx: mpsc::UnboundedReceiver<FramedMessage<<Codec as Encoder>::Item>>,
    pub(crate) sink: Sink<<Codec as Encoder>::Item>,
}

impl<Io, St, Codec: Encoder + Decoder> ConnectResult<Io, St, Codec> {
    #[inline]
    pub fn sink(&self) -> &Sink<<Codec as Encoder>::Item> {
        &self.sink
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
