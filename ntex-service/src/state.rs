use std::marker::PhantomData;

pub trait State<St, Req> {
    fn on_req(&self, _: &St, _: &Req) -> Option<St> {
        None
    }
}

pub trait StateMapping<From>: Clone + 'static {
    type State: 'static;
    type Control;

    fn map<Req>(&self, st: &From) -> (Self::State, Self::Control)
    where
        Self::Control: State<Self::State, Req>;
}

impl<Req> State<(), Req> for () {
    fn on_req(&self, _s: &(), _r: &Req) -> Option<()> {
        None
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Noop;

impl<St, Req> State<St, Req> for Noop {}

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
    type Control = Noop;

    fn map<R>(&self, _: &From) -> (St, Noop) {
        (St::default(), Noop)
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
    type Control = Noop;

    fn map<R>(&self, st: &St) -> (St, Noop) {
        (st.clone(), Noop)
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
    type Control = Noop;

    fn map<R>(&self, _: &From) -> (St, Noop) {
        ((self.0)(), Noop)
    }
}

// SAFETY: Send cannot be provided authomatically because of St param
// but code get executed in one thread and never leave it
unsafe impl<St> Send for DefaultState<St> {}
unsafe impl<St> Send for CloneState<St> {}
unsafe impl<F, St> Send for FnState<F, St> where F: Send {}
