pub trait State<St, Req> {
    fn on_req(&self, _: &St, _: &Req) -> Option<St> {
        None
    }
}

impl<Req> State<(), Req> for () {
    fn on_req(&self, _s: &(), _r: &Req) -> Option<()> {
        None
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Noop;

impl<St, Req> State<St, Req> for Noop {}

pub trait StateMapping<St, From>: Clone + 'static {
    type Control;

    fn map<Req>(&self, st: &From) -> (St, Self::Control)
    where
        Self::Control: State<St, Req>;
}

#[derive(Copy, Clone, Debug)]
pub struct DefaultState;

impl<St: Default, From> StateMapping<St, From> for DefaultState {
    type Control = Noop;

    fn map<R>(&self, _: &From) -> (St, Noop) {
        (St::default(), Noop)
    }
}

#[derive(Copy, Clone, Debug)]
pub struct CloneState;

impl<St: Clone> StateMapping<St, St> for CloneState {
    type Control = Noop;

    fn map<R>(&self, st: &St) -> (St, Noop) {
        (st.clone(), Noop)
    }
}

// SAFETY: Send cannot be provided authomatically because of St and From params
// but code get executed in one thread and never leave it
// unsafe impl<St, Chained> Send for StateMapping<St, Chained> {}
