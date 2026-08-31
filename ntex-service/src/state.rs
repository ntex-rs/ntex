use std::marker::PhantomData;

pub trait State<Req>: Sized + 'static {
    fn on_req(&self, _: &Req) -> Option<Self> {
        None
    }
}

pub trait StateMapping<From>: Clone + 'static {
    type State: 'static;

    fn map(&self, st: &From) -> Self::State;
}

impl<Req> State<Req> for () {}

#[derive(Debug, Default)]
pub struct DefaultState<St>(PhantomData<St>);

impl<St> DefaultState<St> {
    #[inline]
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<St> Copy for DefaultState<St> {}

impl<St> Clone for DefaultState<St> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<St: Default + 'static, From> StateMapping<From> for DefaultState<St> {
    type State = St;

    fn map(&self, _: &From) -> St {
        St::default()
    }
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

pub(crate) trait StateWrapper<St, Req>: Sized + 'static {
    fn get(&self) -> &St;

    fn on_req(&self, _: &Req) -> Option<St>;
}

pub(crate) struct StateWrapperNoReq<St>(pub(crate) St);

impl<St: 'static, Req> StateWrapper<St, Req> for StateWrapperNoReq<St> {
    fn get(&self) -> &St {
        &self.0
    }

    fn on_req(&self, _: &Req) -> Option<St> {
        None
    }
}

pub(crate) struct StateWrapperReq<St>(pub(crate) St);

impl<St: State<Req>, Req> StateWrapper<St, Req> for StateWrapperReq<St> {
    fn get(&self) -> &St {
        &self.0
    }

    fn on_req(&self, req: &Req) -> Option<St> {
        self.0.on_req(req)
    }
}

// SAFETY: Send cannot be provided authomatically because of St param
// but code get executed in one thread and never leave it
unsafe impl<St> Send for DefaultState<St> {}
unsafe impl<St> Send for CloneState<St> {}
unsafe impl<F, St> Send for FnState<F, St> where F: Send {}
