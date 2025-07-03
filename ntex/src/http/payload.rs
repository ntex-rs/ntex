use std::{fmt, future::poll_fn, mem, pin::Pin, task::Context, task::Poll};

use super::{error::PayloadError, h1, h2};
use crate::util::{Bytes, Stream};

/// Type represent boxed payload
pub type PayloadStream = Pin<Box<dyn Stream<Item = Result<Bytes, PayloadError>>>>;

/// Type represent streaming payload
#[derive(Default)]
pub enum Payload {
    #[default]
    None,
    H1(h1::Payload),
    H2(h2::Payload),
    Stream(PayloadStream),
}

impl From<h1::Payload> for Payload {
    fn from(v: h1::Payload) -> Self {
        Payload::H1(v)
    }
}

impl From<h2::Payload> for Payload {
    fn from(v: h2::Payload) -> Self {
        Payload::H2(v)
    }
}

impl From<PayloadStream> for Payload {
    fn from(pl: PayloadStream) -> Self {
        Payload::Stream(pl)
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Payload::None => write!(f, "Payload::None"),
            Payload::H1(ref pl) => write!(f, "Payload::H1({pl:?})"),
            Payload::H2(ref pl) => write!(f, "Payload::H2({pl:?})"),
            Payload::Stream(_) => write!(f, "Payload::Stream(..)"),
        }
    }
}

impl Payload {
    /// Takes current payload and replaces it with `None` value
    pub fn take(&mut self) -> Self {
        mem::take(self)
    }

    /// Create payload from stream
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, PayloadError>> + 'static,
    {
        Payload::Stream(Box::pin(stream))
    }

    #[inline]
    /// Attempt to pull out the next value of this payload.
    pub async fn recv(&mut self) -> Option<Result<Bytes, PayloadError>> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    #[inline]
    /// Attempt to pull out the next value of this payload, registering
    /// the current task for wakeup if the value is not yet available,
    /// and returning None if the payload is exhausted.
    pub fn poll_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, PayloadError>>> {
        match self {
            Payload::None => Poll::Ready(None),
            Payload::H1(ref mut pl) => pl.poll_read(cx),
            Payload::H2(ref mut pl) => pl.poll_read(cx),
            Payload::Stream(ref mut pl) => Pin::new(pl).poll_next(cx),
        }
    }
}

impl Stream for Payload {
    type Item = Result<Bytes, PayloadError>;

    #[inline]
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_debug() {
        assert!(format!("{:?}", Payload::None).contains("Payload::None"));
        assert!(
            format!("{:?}", Payload::H1(crate::channel::bstream::channel().1))
                .contains("Payload::H1")
        );
        assert!(format!(
            "{:?}",
            Payload::Stream(Box::pin(crate::channel::bstream::channel().1))
        )
        .contains("Payload::Stream"));
    }
}
