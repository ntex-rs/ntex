use std::marker::PhantomData;

use ntex_service::{chain_factory, fn_service, Service, ServiceCtx, ServiceFactory};
use ntex_util::future::Ready;

use crate::{Filter, FilterFactory, Io, IoBoxed, Layer};

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

/// Create filter factory service
pub fn filter<T, F>(filter: T) -> FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
{
    FilterServiceFactory {
        filter,
        _t: PhantomData,
    }
}

pub struct FilterServiceFactory<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> ServiceFactory<Io<F>> for FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
{
    type Response = Io<Layer<T::Filter, F>>;
    type Error = T::Error;
    type Service = FilterService<T, F>;
    type InitError = ();
    type Future<'f> = Ready<Self::Service, Self::InitError> where Self: 'f;

    #[inline]
    fn create(&self, _: ()) -> Self::Future<'_> {
        Ready::Ok(FilterService {
            filter: self.filter.clone(),
            _t: PhantomData,
        })
    }
}

pub struct FilterService<T, F> {
    filter: T,
    _t: PhantomData<F>,
}

impl<T, F> Service<Io<F>> for FilterService<T, F>
where
    T: FilterFactory<F> + Clone,
{
    type Response = Io<Layer<T::Filter, F>>;
    type Error = T::Error;
    type Future<'f> = T::Future where T: 'f, F: 'f;

    #[inline]
    fn call<'a>(&'a self, req: Io<F>, _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        self.filter.clone().create(req)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ntex_bytes::Bytes;
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{
        buf::Stack, filter::NullFilter, testing::IoTest, FilterLayer, ReadBuf, WriteBuf,
    };

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

    pub(crate) struct TestFilter;

    impl FilterLayer for TestFilter {
        fn process_read_buf(&self, buf: &ReadBuf<'_>) -> io::Result<usize> {
            Ok(buf.nbytes())
        }

        fn process_write_buf(&self, _: &WriteBuf<'_>) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Copy, Clone, Debug)]
    struct TestFilterFactory;

    impl<F: Filter> FilterFactory<F> for TestFilterFactory {
        type Filter = TestFilter;
        type Error = std::convert::Infallible;
        type Future = Ready<Io<Layer<TestFilter, F>>, Self::Error>;

        fn create(self, st: Io<F>) -> Self::Future {
            Ready::Ok(st.add_filter(TestFilter))
        }
    }

    #[ntex::test]
    async fn test_utils_filter() {
        let (_, server) = IoTest::create();
        let svc = chain_factory(
            filter::<_, crate::filter::Base>(TestFilterFactory)
                .map_err(|_| ())
                .map_init_err(|_| ()),
        )
        .and_then(seal(fn_service(|io: IoBoxed| async move {
            let _ = io.recv(&BytesCodec).await;
            Ok::<_, ()>(())
        })))
        .pipeline(())
        .await
        .unwrap();
        let _ = svc.call(Io::new(server)).await;
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
            ntex_util::future::poll_fn(|cx| NullFilter.poll_read_ready(cx)).await,
            crate::ReadStatus::Terminate
        );
        assert_eq!(
            ntex_util::future::poll_fn(|cx| NullFilter.poll_write_ready(cx)).await,
            crate::WriteStatus::Terminate
        );
        assert!(NullFilter.process_write_buf(&ioref, &stack, 0).is_ok());
        assert_eq!(
            NullFilter.process_read_buf(&ioref, &stack, 0, 0).unwrap(),
            Default::default()
        )
    }
}
