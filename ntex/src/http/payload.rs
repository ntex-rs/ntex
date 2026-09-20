use std::{fmt, future::poll_fn, mem, pin::Pin, task::Context, task::Poll};

use crate::http::{error::PayloadError, h1, h2};
use crate::util::{Bytes, Stream};

/// A boxed stream of HTTP payload chunks.
pub type PayloadStream = Pin<Box<dyn Stream<Item = Result<Bytes, PayloadError>>>>;

/// An HTTP request payload.
#[derive(Default)]
pub enum Payload {
    /// No payload is available.
    #[default]
    None,
    /// An HTTP/1 payload stream.
    H1(h1::Payload),
    /// An HTTP/2 payload stream.
    H2(h2::Payload),
    /// A custom payload stream.
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
            Payload::H1(pl) => write!(f, "Payload::H1({pl:?})"),
            Payload::H2(pl) => write!(f, "Payload::H2({pl:?})"),
            Payload::Stream(_) => write!(f, "Payload::Stream(..)"),
        }
    }
}

impl Payload {
    #[must_use]
    /// Takes the payload and replaces it with [`Payload::None`].
    pub fn take(&mut self) -> Self {
        mem::take(self)
    }

    #[must_use]
    /// Creates a payload from a local asynchronous byte stream.
    ///
    /// The stream is pinned and does not need to implement [`Unpin`]. It must
    /// yield HTTP [`PayloadError`] values directly.
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, PayloadError>> + 'static,
    {
        Payload::Stream(Box::pin(stream))
    }

    #[inline]
    /// Waits for and returns the next payload chunk.
    pub async fn recv(&mut self) -> Option<Result<Bytes, PayloadError>> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    #[inline]
    /// Polls for the next payload chunk.
    ///
    /// Registers the current task for wakeup when data is not yet available
    /// and returns `Poll::Ready(None)` after the payload is exhausted.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, PayloadError>>> {
        match self {
            Payload::None => Poll::Ready(None),
            Payload::H1(pl) => pl.poll_read(cx),
            Payload::H2(pl) => pl.poll_read(cx),
            Payload::Stream(pl) => Pin::new(pl).poll_next(cx),
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
        assert!(
            format!(
                "{:?}",
                Payload::Stream(Box::pin(crate::channel::bstream::channel().1))
            )
            .contains("Payload::Stream")
        );

        assert_eq!(
            std::mem::size_of::<Payload>(),
            std::mem::size_of::<Option<Payload>>()
        );
    }
}
