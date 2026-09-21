use std::{error, fmt, future::Future, io, pin::Pin, task::Context, task::Poll};

use ntex_service::{Ctx, Service};

/// Combines two different futures, streams, or sinks having the same associated types into a single
/// type.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Either<A, B> {
    /// First branch of the type
    Left(A),
    /// Second branch of the type
    Right(B),
}

impl<A, B> Either<A, B> {
    fn project(self: Pin<&mut Self>) -> Either<Pin<&mut A>, Pin<&mut B>> {
        unsafe {
            match self.get_unchecked_mut() {
                Either::Left(a) => Either::Left(Pin::new_unchecked(a)),
                Either::Right(b) => Either::Right(Pin::new_unchecked(b)),
            }
        }
    }

    #[inline]
    /// Return true if the value is the `Left` variant.
    pub fn is_left(&self) -> bool {
        match *self {
            Either::Left(_) => true,
            Either::Right(_) => false,
        }
    }

    #[inline]
    /// Return true if the value is the `Right` variant.
    pub fn is_right(&self) -> bool {
        !self.is_left()
    }

    #[inline]
    /// Convert the left side of `Either<L, R>` to an `Option<L>`.
    pub fn left(self) -> Option<A> {
        match self {
            Either::Left(l) => Some(l),
            Either::Right(_) => None,
        }
    }

    #[inline]
    /// Convert the right side of `Either<L, R>` to an `Option<R>`.
    pub fn right(self) -> Option<B> {
        match self {
            Either::Left(_) => None,
            Either::Right(r) => Some(r),
        }
    }

    #[inline]
    /// Convert `&Either<L, R>` to `Either<&L, &R>`.
    pub fn as_ref(&self) -> Either<&A, &B> {
        match *self {
            Either::Left(ref inner) => Either::Left(inner),
            Either::Right(ref inner) => Either::Right(inner),
        }
    }

    #[inline]
    /// Convert `&mut Either<L, R>` to `Either<&mut L, &mut R>`.
    pub fn as_mut(&mut self) -> Either<&mut A, &mut B> {
        match *self {
            Either::Left(ref mut inner) => Either::Left(inner),
            Either::Right(ref mut inner) => Either::Right(inner),
        }
    }
}

impl<T> Either<T, T> {
    #[inline]
    /// Extract the value of an either over two equivalent types.
    pub fn into_inner(self) -> T {
        match self {
            Either::Left(x) | Either::Right(x) => x,
        }
    }
}

/// `Either` implements `Error` if *both* `A` and `B` implement it.
impl<A, B> error::Error for Either<A, B>
where
    A: error::Error,
    B: error::Error,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Either::Left(a) => a.source(),
            Either::Right(b) => b.source(),
        }
    }
}

impl<A, B> fmt::Display for Either<A, B>
where
    A: fmt::Display,
    B: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Either::Left(a) => a.fmt(f),
            Either::Right(b) => b.fmt(f),
        }
    }
}

impl<A, B> Future for Either<A, B>
where
    A: Future,
    B: Future<Output = A::Output>,
{
    type Output = A::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            Either::Left(x) => x.poll(cx),
            Either::Right(x) => x.poll(cx),
        }
    }
}

impl<E: error::Error> From<Either<E, io::Error>> for io::Error {
    fn from(err: Either<E, io::Error>) -> Self {
        match err {
            Either::Left(e) => io::Error::other(format!("{e:?}")),
            Either::Right(e) => e,
        }
    }
}

impl<A, B, St, Req> Service<St, Req> for Either<A, B>
where
    A: Service<St, Req>,
    B: Service<St, Req, Res = A::Res, Error = A::Error>,
{
    type Res = A::Res;
    type Error = A::Error;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        match self {
            Either::Left(svc) => ctx.call(svc, req).await,
            Either::Right(svc) => ctx.call(svc, req).await,
        }
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        match self {
            Either::Left(svc) => ctx.ready(svc).await,
            Either::Right(svc) => ctx.ready(svc).await,
        }
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        match self {
            Either::Left(svc) => ctx.shutdown(svc).await,
            Either::Right(svc) => ctx.shutdown(svc).await,
        }
    }
}

#[cfg(test)]
mod test {
    use std::{cell::Cell, error::Error as _, future, rc::Rc};

    use ntex_service::Pipeline;

    use super::*;

    #[test]
    fn accessors() {
        let mut value = Either::<u8, String>::Left(10);
        assert!(value.is_left());
        assert!(!value.is_right());
        assert_eq!(value.as_ref(), Either::Left(&10));
        if let Either::Left(inner) = value.as_mut() {
            *inner = 20;
        }
        assert_eq!(value.left(), Some(20));

        let mut value = Either::<u8, String>::Right("right".to_owned());
        assert!(!value.is_left());
        assert!(value.is_right());
        assert_eq!(value.as_ref(), Either::Right(&"right".to_owned()));
        if let Either::Right(inner) = value.as_mut() {
            inner.push_str(" branch");
        }
        assert_eq!(value.right().as_deref(), Some("right branch"));

        assert_eq!(Either::<u8, u8>::Left(1).into_inner(), 1);
        assert_eq!(Either::<u8, u8>::Right(2).into_inner(), 2);

        assert_eq!(
            format!("{}", Either::<_, &'static str>::Left("test")),
            "test"
        );
        assert_eq!(
            format!("{}", Either::<&'static str, _>::Right("test")),
            "test"
        );
    }

    #[ntex::test]
    async fn future() {
        let left: Either<_, future::Ready<u8>> = Either::Left(future::ready(10));
        assert_eq!(left.await, 10);

        let right: Either<future::Ready<u8>, _> = Either::Right(future::ready(20));
        assert_eq!(right.await, 20);
    }

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<io::Error>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl error::Error for TestError {
        fn source(&self) -> Option<&(dyn error::Error + 'static)> {
            self.source
                .as_ref()
                .map(|source| source as &(dyn error::Error + 'static))
        }
    }

    #[test]
    fn errors() {
        let left = Either::<TestError, TestError>::Left(TestError {
            message: "left",
            source: Some(io::Error::other("left source")),
        });
        assert_eq!(left.to_string(), "left");
        assert_eq!(left.source().unwrap().to_string(), "left source");

        let right = Either::<TestError, TestError>::Right(TestError {
            message: "right",
            source: Some(io::Error::other("right source")),
        });
        assert_eq!(right.to_string(), "right");
        assert_eq!(right.source().unwrap().to_string(), "right source");

        let left: io::Error = Either::<TestError, io::Error>::Left(TestError {
            message: "converted",
            source: None,
        })
        .into();
        assert_eq!(left.kind(), io::ErrorKind::Other);
        assert!(left.to_string().contains("converted"));

        let right: io::Error = Either::<TestError, io::Error>::Right(io::Error::new(
            io::ErrorKind::TimedOut,
            "right error",
        ))
        .into();
        assert_eq!(right.kind(), io::ErrorKind::TimedOut);
        assert_eq!(right.to_string(), "right error");
    }

    #[derive(Default)]
    struct ServiceState {
        ready: Cell<bool>,
        called: Cell<bool>,
        shutdown: Cell<bool>,
    }

    struct TestService {
        state: Rc<ServiceState>,
        response: u8,
    }

    impl Service<(), u8> for TestService {
        type Res = u8;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
            self.state.ready.set(true);
            Ok(())
        }

        async fn call(&self, req: u8, _: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
            self.state.called.set(true);
            Ok(req + self.response)
        }

        async fn shutdown(&self, _: Ctx<'_, Self>) {
            self.state.shutdown.set(true);
        }
    }

    #[ntex::test]
    async fn service() {
        async fn check(service: Either<TestService, TestService>, response: u8) {
            let state = match &service {
                Either::Left(service) | Either::Right(service) => service.state.clone(),
            };
            let service = Pipeline::new((), service);

            assert_eq!(service.ready().await, Ok(()));
            assert!(state.ready.get());
            assert_eq!(service.call(5).await, Ok(5 + response));
            assert!(state.called.get());
            service.shutdown().await;
            assert!(state.shutdown.get());
        }

        let state = Rc::new(ServiceState::default());
        check(
            Either::Left(TestService {
                state,
                response: 10,
            }),
            10,
        )
        .await;

        let state = Rc::new(ServiceState::default());
        check(
            Either::Right(TestService {
                state,
                response: 20,
            }),
            20,
        )
        .await;
    }
}
