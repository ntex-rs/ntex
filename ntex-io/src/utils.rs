use ntex_service::{chain_factory, fn_service, ServiceFactory};
use ntex_util::future::Ready;

use crate::{Filter, Io, IoBoxed};

/// Decoded item from buffer
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Decoded<T> {
    pub item: Option<T>,
    pub remains: usize,
    pub consumed: usize,
}

/// Service that converts any Io<F> stream to IoBoxed stream
pub fn seal<F, S, C>(
    srv: S,
) -> impl ServiceFactory<
    Io<F>,
    C,
    Response = S::Response,
    Error = S::Error,
    InitError = S::InitError,
>
where
    F: Filter,
    S: ServiceFactory<IoBoxed, C>,
    C: Clone,
{
    chain_factory(fn_service(|io: Io<F>| Ready::Ok(IoBoxed::from(io))))
        .map_init_err(|_| panic!())
        .and_then(srv)
}

#[cfg(test)]
mod tests {
    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{buf::Stack, filter::NullFilter, testing::IoTest};

    #[ntex::test]
    async fn test_utils() {
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
        let _ = svc.call(Io::new(server)).await;

        let buf = client.read().await.unwrap();
        assert_eq!(buf, b"RES".as_ref());
    }

    #[ntex::test]
    async fn test_null_filter() {
        let (_, server) = IoTest::create();
        let io = Io::new(server);
        let ioref = io.get_ref();
        let stack = Stack::new();
        assert!(NullFilter.query(std::any::TypeId::of::<()>()).is_none());
        assert!(NullFilter.shutdown(&ioref, &stack, 0).unwrap().is_ready());
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_read_ready(cx)).await,
            crate::ReadStatus::Terminate
        );
        assert_eq!(
            std::future::poll_fn(|cx| NullFilter.poll_write_ready(cx)).await,
            crate::WriteStatus::Terminate
        );
        assert!(NullFilter.process_write_buf(&ioref, &stack, 0).is_ok());
        assert_eq!(
            NullFilter.process_read_buf(&ioref, &stack, 0, 0).unwrap(),
            Default::default()
        )
    }
}
