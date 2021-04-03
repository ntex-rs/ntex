use std::{error, fmt, future::Future, pin::Pin, task::Context, task::Poll};

/// Combines two different futures, streams, or sinks having the same associated types into a single
/// type.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Either<A, B> {
    /// First branch of the type
    Left(/* #[pin] */ A),
    /// Second branch of the type
    Right(/* #[pin] */ B),
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

impl<A, B, T> Either<(T, A), (T, B)> {
    #[inline]
    /// Factor out a homogeneous type from an either of pairs.
    ///
    /// Here, the homogeneous type is the first element of the pairs.
    pub fn factor_first(self) -> (T, Either<A, B>) {
        match self {
            Either::Left((x, a)) => (x, Either::Left(a)),
            Either::Right((x, b)) => (x, Either::Right(b)),
        }
    }
}

impl<A, B, T> Either<(A, T), (B, T)> {
    #[inline]
    /// Factor out a homogeneous type from an either of pairs.
    ///
    /// Here, the homogeneous type is the second element of the pairs.
    pub fn factor_second(self) -> (Either<A, B>, T) {
        match self {
            Either::Left((a, x)) => (Either::Left(a), x),
            Either::Right((b, x)) => (Either::Right(b), x),
        }
    }
}

impl<T> Either<T, T> {
    #[inline]
    /// Extract the value of an either over two equivalent types.
    pub fn into_inner(self) -> T {
        match self {
            Either::Left(x) => x,
            Either::Right(x) => x,
        }
    }
}

/// `Either` implements `Error` if *both* `A` and `B` implement it.
impl<A, B> error::Error for Either<A, B>
where
    A: error::Error,
    B: error::Error,
{
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
