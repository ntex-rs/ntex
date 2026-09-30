use std::{fmt, future::poll_fn, mem, pin::Pin, task::Context, task::Poll};

use crate::channel::bstream;
use crate::http::{HeaderMap, error::PayloadError, h1, h2};
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

impl From<bstream::Receiver<PayloadError>> for Payload {
    fn from(v: bstream::Receiver<PayloadError>) -> Self {
        Payload::H1(v.into())
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

    /// Returns the trailer fields received at the end of the payload.
    ///
    /// Trailers are available after the payload is complete. HTTP/1 chunked
    /// and HTTP/2 payloads provide trailers.
    pub fn trailers(&self) -> Option<HeaderMap> {
        match self {
            Payload::H1(pl) => pl.trailers(),
            Payload::H2(pl) => pl.trailers(),
            _ => None,
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
            format!(
                "{:?}",
                Payload::H1(crate::channel::bstream::channel().1.into())
            )
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

    #[crate::rt_test]
    async fn payload_conversions() {
        use crate::http::h1;

        let (tx, rx) = crate::channel::bstream::channel();
        let mut pl = Payload::from(h1::Payload::from(rx));
        assert!(matches!(pl, Payload::H1(_)));
        tx.feed_data(Bytes::from_static(b"data"));
        tx.feed_eof();
        assert_eq!(pl.recv().await.unwrap().unwrap(), "data");
        assert!(pl.recv().await.is_none());
        assert!(pl.trailers().is_none());

        let mut pl = Payload::None;
        assert!(pl.recv().await.is_none());
        assert!(pl.trailers().is_none());

        let (tx, rx) = crate::channel::bstream::channel();
        let pl: PayloadStream = Box::pin(rx);
        let mut pl = Payload::from(pl);
        assert!(matches!(pl, Payload::Stream(_)));
        tx.feed_eof();
        assert!(pl.recv().await.is_none());
        assert!(pl.trailers().is_none());
        assert!(matches!(pl.take(), Payload::Stream(_)));
        assert!(matches!(pl, Payload::None));
    }
}
