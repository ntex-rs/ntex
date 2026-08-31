use std::marker::PhantomData;

pub trait StateMapping<From>: Clone + 'static {
    type State: 'static;

    fn map(&self, st: &From) -> Self::State;
}

#[derive(Debug, Default)]
pub struct CloneState<St>(PhantomData<St>);

impl<St> CloneState<St> {
    #[inline]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<St> Copy for CloneState<St> {}

impl<St> Clone for CloneState<St> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<St: Clone + 'static> StateMapping<St> for CloneState<St> {
    type State = St;

    fn map(&self, st: &St) -> St {
        st.clone()
    }
}

#[derive(Debug)]
pub struct FnState<F, St>(F, PhantomData<St>);

impl<F, St> FnState<F, St>
where
    F: Fn() -> St + Clone,
{
    #[inline]
    pub fn new(f: F) -> Self {
        Self(f, PhantomData)
    }
}

impl<F: Clone, St> Clone for FnState<F, St> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), PhantomData)
    }
}

impl<F, St, From> StateMapping<From> for FnState<F, St>
where
    F: Fn() -> St + Clone + 'static,
    St: 'static,
{
    type State = St;

    fn map(&self, _: &From) -> St {
        (self.0)()
    }
}

pub trait RequestState {
    type Req;
    type Res;
    type State: 'static;
    type Error;

    async fn map(&self, req: Self::Req) -> Result<(Self::Res, Self::State), Self::Error>;
}

#[derive(Debug)]
pub struct DefaultState<Req, Err>(PhantomData<(Req, Err)>);

impl<Req, Err> DefaultState<Req, Err> {
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<Req, Err> Copy for DefaultState<Req, Err> {}

impl<Req, Err> Clone for DefaultState<Req, Err> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<Req, Err> RequestState for DefaultState<Req, Err> {
    type Req = Req;
    type Res = Req;
    type State = ();
    type Error = Err;

    async fn map(&self, req: Req) -> Result<(Self::Req, Self::State), Self::Error> {
        Ok((req, ()))
    }
}

// SAFETY: Send cannot be provided authomatically because of St param
// but code get executed in one thread and never leave it
unsafe impl<Req, Err> Send for DefaultState<Req, Err> {}
unsafe impl<St> Send for CloneState<St> {}
unsafe impl<F, St> Send for FnState<F, St> where F: Send {}
