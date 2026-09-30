use std::{any::Any, any::TypeId, fmt, io, ops, task::Context, task::Poll};

use crate::{Filter, FilterCtx, Io, Readiness};

/// Type-erased filter chain used by [`IoBoxed`].
pub struct Sealed(pub(crate) Box<dyn Filter>);

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed").finish()
    }
}

impl Filter for Sealed {
    #[inline]
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        self.0.query(id)
    }

    #[inline]
    fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        self.0.process_read_buf(ctx)
    }

    #[inline]
    fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
        self.0.process_write_buf(ctx)
    }

    #[inline]
    fn shutdown(&self, ctx: &mut FilterCtx<'_>) -> io::Result<Poll<()>> {
        self.0.shutdown(ctx)
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        self.0.poll_write_ready(cx)
    }
}

#[derive(Debug)]
/// An [`Io`] object whose filter-chain type has been erased.
pub struct IoBoxed(Io<Sealed>);

impl IoBoxed {
    #[inline]
    #[must_use]
    /// Transfers the live I/O state into a new object.
    ///
    /// This does not clone the connection. The current object is replaced with
    /// a stopped placeholder and should no longer be used for I/O.
    pub fn take(&mut self) -> Self {
        IoBoxed(self.0.take())
    }
}

impl<F: Filter> From<Io<F>> for IoBoxed {
    fn from(io: Io<F>) -> Self {
        Self(io.seal())
    }
}

impl ops::Deref for IoBoxed {
    type Target = Io<Sealed>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<IoBoxed> for Io<Sealed> {
    fn from(value: IoBoxed) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{FilterBuf, FilterLayer, filter::NullFilter, testing::IoTest};

    #[derive(Debug)]
    struct Tagged;

    impl FilterLayer for Tagged {
        fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
            (id == TypeId::of::<&'static str>()).then(|| Box::new("tagged") as Box<dyn Any>)
        }

        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src.take() {
                    dst.extend_from_slice(&src);
                }
            });
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(ntex_bytes::BytePages::move_to);
            Ok(())
        }
    }

    /// Filter chain wrapper built from the forwarding macros.
    struct Wrapper<F> {
        inner: F,
    }

    impl<F: Filter> Filter for Wrapper<F> {
        crate::forward_ready!(inner);
        crate::forward_query!(inner);
        crate::forward_shutdown!(inner);

        fn process_read_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.inner.process_read_buf(ctx)
        }

        fn process_write_buf(&self, ctx: &mut FilterCtx<'_>) -> io::Result<()> {
            self.inner.process_write_buf(ctx)
        }
    }

    #[ntex::test]
    async fn sealed_chain_delegates_to_inner_filter() {
        assert_eq!(format!("{:?}", Sealed(Box::new(NullFilter))), "Sealed");

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);

        let io = Io::from(server).add_filter(Tagged).boxed();
        let io: Io<Sealed> = io.into();
        let io = io.map_filter(|inner| Wrapper { inner });
        assert_eq!(io.query::<&'static str>().get(), Some("tagged"));
        assert!(io.query::<u32>().get().is_none());

        client.write("hello");
        let item = io.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(item, Bytes::from_static(b"hello"));

        io.send(Bytes::from_static(b"world"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(client.read().await.unwrap(), Bytes::from_static(b"world"));

        io.shutdown().await.unwrap();
        assert!(io.is_closed());
    }
}
