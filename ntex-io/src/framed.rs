use std::{fmt, io};

use ntex_codec::{Decoder, Encoder};
use ntex_util::future::Either;

use crate::Io;

/// A unified interface to an underlying I/O object, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
/// `Framed` is heavily optimized for streaming io.
pub struct Framed<F, U> {
    io: Io<F>,
    codec: U,
}

impl<F, U> Framed<F, U>
where
    U: Decoder + Encoder,
{
    #[inline]
    /// Provides an interface for reading and writing to
    /// `Io` object, using `Decode` and `Encode` traits of codec.
    pub fn new(io: Io<F>, codec: U) -> Framed<F, U> {
        Framed { io, codec }
    }

    #[inline]
    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    pub fn get_io(&self) -> &Io<F> {
        &self.io
    }

    #[inline]
    /// Returns a reference to the underlying codec.
    pub fn get_codec(&self) -> &U {
        &self.codec
    }

    #[inline]
    /// Wake write task and instruct to flush data.
    ///
    /// This is async version of .poll_flush() method.
    pub async fn flush(&self, full: bool) -> Result<(), io::Error> {
        self.io.flush(full).await
    }

    #[inline]
    /// Shut down io stream
    pub async fn shutdown(&self) -> Result<(), io::Error> {
        self.io.shutdown().await
    }
}

impl<F, U> Framed<F, U>
where
    U: Decoder,
{
    #[inline]
    /// Read incoming io stream and decode codec item.
    pub async fn recv(&self) -> Result<Option<U::Item>, Either<U::Error, io::Error>> {
        self.io.recv(&self.codec).await
    }
}

impl<F, U> fmt::Debug for Framed<F, U>
where
    Io<F>: fmt::Debug,
    U: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Framed")
            .field("io", &self.io)
            .field("codec", &self.codec)
            .finish()
    }
}

impl<F, U> Framed<F, U>
where
    U: Encoder,
{
    #[inline]
    /// Serialize item and Write to the inner buffer
    pub async fn send(
        &self,
        item: <U as Encoder>::Item,
    ) -> Result<(), Either<U::Error, io::Error>> {
        self.io.send(&self.codec, item).await
    }
}

#[cfg(test)]
mod tests {
    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{testing::IoTest, Io};

    #[ntex::test]
    async fn framed() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write(b"chunk-0");

        let server = Framed::new(Io::new(server), BytesCodec);
        server.get_codec();
        server.get_io();
        assert!(format!("{:?}", server).contains("Framed"));

        let item = server.recv().await.unwrap().unwrap();
        assert_eq!(item, b"chunk-0".as_ref());

        let data = Bytes::from_static(b"chunk-1");
        server.send(data).await.unwrap();
        server.flush(true).await.unwrap();
        assert_eq!(client.read_any(), b"chunk-1".as_ref());

        server.shutdown().await.unwrap();
        assert!(client.is_closed());
    }
}
