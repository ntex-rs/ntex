use ntex_util::future::Ready;
use std::{future::Future, marker::PhantomData};

use crate::{apply_fn, dev::Apply, Service, Transform};

/// Use function as transform service
pub fn fn_transform<S, F, R, Req, Res, Err>(
    f: F,
) -> impl Transform<S, Request = Req, Response = Res, Error = Err, InitError = ()> + Clone
where
    S: Service<Error = Err>,
    F: Fn(Req, &S) -> R + Clone,
    R: Future<Output = Result<Res, Err>>,
{
    FnTransform::new(f)
}

pub struct FnTransform<S, F, R, Req, Res, Err>
where
    S: Service<Error = Err>,
    F: Fn(Req, &S) -> R + Clone,
    R: Future<Output = Result<Res, Err>>,
{
    f: F,
    _t: PhantomData<(S, R, Req)>,
}

impl<S, F, R, Req, Res, Err> FnTransform<S, F, R, Req, Res, Err>
where
    S: Service<Error = Err>,
    F: Fn(Req, &S) -> R + Clone,
    R: Future<Output = Result<Res, Err>>,
{
    fn new(f: F) -> Self {
        FnTransform { f, _t: PhantomData }
    }
}

impl<S, F, R, Req, Res, Err> Transform<S> for FnTransform<S, F, R, Req, Res, Err>
where
    S: Service<Error = Err>,
    F: Fn(Req, &S) -> R + Clone,
    R: Future<Output = Result<Res, Err>>,
{
    type Request = Req;
    type Response = Res;
    type Error = Err;
    type Transform = Apply<S, F, R, Req, Res, Err>;
    type InitError = ();
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(apply_fn(service, self.f.clone()))
    }
}

impl<S, F, R, Req, Res, Err> Clone for FnTransform<S, F, R, Req, Res, Err>
where
    S: Service<Error = Err>,
    F: Fn(Req, &S) -> R + Clone,
    R: Future<Output = Result<Res, Err>>,
{
    fn clone(&self) -> Self {
        Self::new(self.f.clone())
    }
}

#[cfg(test)]
#[allow(clippy::redundant_clone)]
mod tests {
    use ntex_util::future::lazy;
    use std::task::{Context, Poll};

    use super::*;
    use crate::Service;

    #[derive(Clone)]
    struct Srv;

    impl Service for Srv {
        type Request = usize;
        type Response = usize;
        type Error = ();
        type Future = Ready<usize, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&self, i: usize) -> Self::Future {
            Ready::Ok(i * 2)
        }
    }

    #[ntex::test]
    async fn transform() {
        let srv =
            fn_transform::<Srv, _, _, _, _, _>(|i: usize, srv: &_| srv.call(i + 1))
                .clone()
                .new_transform(Srv)
                .await
                .unwrap();

        let res = srv.call(10usize).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 22);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = lazy(|cx| srv.poll_shutdown(cx, true)).await;
        assert_eq!(res, Poll::Ready(()));
    }
}
