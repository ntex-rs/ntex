use std::{any::Any, any::TypeId, fmt, io, ops, task::Context, task::Poll};

use crate::filter::{Filter, FilterReadStatus};
use crate::{FilterCtx, Io, Readiness};

/// Sealed filter type
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
    fn process_read_buf(
        &self,
        ctx: FilterCtx<'_>,
        nbytes: usize,
    ) -> io::Result<FilterReadStatus> {
        self.0.process_read_buf(ctx, nbytes)
    }

    #[inline]
    fn process_write_buf(&self, ctx: FilterCtx<'_>) -> io::Result<()> {
        self.0.process_write_buf(ctx)
    }

    #[inline]
    fn shutdown(&self, ctx: FilterCtx<'_>) -> io::Result<Poll<()>> {
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
/// Boxed `Io` object with erased filter type
pub struct IoBoxed(Io<Sealed>);

impl IoBoxed {
    #[inline]
    #[must_use]
    /// Clone current io object.
    ///
    /// Current io object becomes closed.
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
    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;
    use ntex_service::{ServiceFactory, cfg::SharedCfg, fn_service};

    use super::*;
    use crate::{testing::IoTest, utils::seal};

    #[ntex::test]
    async fn test_seal() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        client.write("REQ");

        let svc = seal(fn_service(|io: IoBoxed| async move {
            let t = io.recv(&BytesCodec).await.unwrap().unwrap();
            assert_eq!(t, b"REQ".as_ref());
            io.send(Bytes::from_static(b"RES"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, ()>(())
        }))
        .pipeline(())
        .await
        .unwrap();

        let srv: Io<Sealed> = Io::new(server, SharedCfg::default()).boxed().into();
        let _ = svc.call(srv).await;

        let buf = client.read().await.unwrap();
        assert_eq!(buf, b"RES".as_ref());
    }
}
