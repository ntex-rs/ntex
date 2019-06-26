use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use futures::unsync::mpsc;

use crate::cell::Cell;
use crate::dispatcher::FramedMessage;
use crate::sink::Sink;

pub struct Connect<Io, St = (), Codec = ()> {
    io: Io,
    codec: Codec,
    state: St,
    // rx: mpsc::UnboundedReceiver<FramedMessage<<Codec as Encoder>::Item>>,
    // sink: Sink<<Codec as Encoder>::Item>,
}

impl<Io> Connect<Io> {
    pub(crate) fn new(io: Io) -> Self {
        Self {
            io,
            codec: (),
            state: (),
        }
    }
}

impl<Io, S, C> Connect<Io, S, C> {
    pub fn codec<Codec>(self, codec: Codec) -> Connect<Io, S, Codec> {
        Connect {
            codec,
            io: self.io,
            state: self.state,
        }
    }

    pub fn state<St>(self, state: St) -> Connect<Io, St, C> {
        Connect {
            state,
            io: self.io,
            codec: self.codec,
        }
    }

    pub fn state_fn<St, F>(self, f: F) -> Connect<Io, St, C>
    where
        F: FnOnce(&Connect<Io, S, C>) -> St,
    {
        Connect {
            state: f(&self),
            io: self.io,
            codec: self.codec,
        }
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
            framed: Framed::new(self.io, self.codec),
            rx,
            sink,
        }
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
