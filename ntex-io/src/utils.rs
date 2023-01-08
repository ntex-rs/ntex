use std::marker::PhantomData;

use ntex_service::{fn_service, pipeline_factory, Service, ServiceFactory};
use ntex_util::future::Ready;

use crate::{Filter, FilterFactory, Io, IoBoxed};

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
    pipeline_factory(
        fn_service(|io: Io<F>| Ready::Ok(IoBoxed::from(io))).map_init_err(|_| panic!()),
    )
    .and_then(srv)
}

/// Create filter factory service
pub fn filter<T, F>(filter: T) -> FilterServiceFactory<T, F>
where
    T: FilterFactory<F> + Clone,
    F: Filter,
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
    F: Filter,
{
    type Response = Io<T::Filter>;
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
    F: Filter,
{
    type Response = Io<T::Filter>;
    type Error = T::Error;
    type Future<'f> = T::Future where T: 'f;

    #[inline]
    fn call(&self, req: Io<F>) -> Self::Future<'_> {
        req.add_filter(self.filter.clone())
    }
}

#[cfg(test)]
mod tests {
    use ntex_bytes::{Bytes, BytesVec};
    use ntex_codec::BytesCodec;

    use super::*;
    use crate::{filter::NullFilter, testing::IoTest};

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
        .create(())
        .await
        .unwrap();
        let _ = svc.call(Io::new(server)).await;

        let buf = client.read().await.unwrap();
        assert_eq!(buf, b"RES".as_ref());
    }

    #[derive(Copy, Clone, Debug)]
    struct NullFilterFactory;

    impl<F: Filter> FilterFactory<F> for NullFilterFactory {
        type Filter = crate::filter::NullFilter;
        type Error = std::convert::Infallible;
        type Future = Ready<Io<Self::Filter>, Self::Error>;

        fn create(self, st: Io<F>) -> Self::Future {
            st.map_filter(|_| Ok(NullFilter)).into()
        }
    }

    #[ntex::test]
    async fn test_utils_filter() {
        let (_, server) = IoTest::create();
        let svc = pipeline_factory(
            filter::<_, crate::filter::Base>(NullFilterFactory)
                .map_err(|_| ())
                .map_init_err(|_| ()),
        )
        .and_then(seal(fn_service(|io: IoBoxed| async move {
            let _ = io.recv(&BytesCodec).await;
            Ok::<_, ()>(())
        })))
        .create(())
        .await
        .unwrap();
        let _ = svc.call(Io::new(server)).await;
    }

    #[ntex::test]
    async fn test_null_filter() {
        assert!(NullFilter.query(std::any::TypeId::of::<()>()).is_none());
        assert!(NullFilter.poll_shutdown().is_ready());
        assert_eq!(
            ntex_util::future::poll_fn(|cx| NullFilter.poll_read_ready(cx)).await,
            crate::ReadStatus::Terminate
        );
        assert_eq!(
            ntex_util::future::poll_fn(|cx| NullFilter.poll_write_ready(cx)).await,
            crate::WriteStatus::Terminate
        );
        assert_eq!(NullFilter.get_read_buf(), None);
        assert_eq!(NullFilter.get_write_buf(), None);
        assert!(NullFilter.release_write_buf(BytesVec::new()).is_ok());
        NullFilter.release_read_buf(BytesVec::new());

        let (_, server) = IoTest::create();
        let io = Io::new(server);
        assert_eq!(
            NullFilter.process_read_buf(&io.get_ref(), 10).unwrap(),
            (0, 0)
        )
    }
}
