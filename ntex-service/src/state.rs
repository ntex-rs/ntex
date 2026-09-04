pub trait RequestState<Res> {
    type State: 'static;

    fn unpack(self) -> (Self::State, Res);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State<St, Req> {
    pub req: Req,
    pub state: St,
}

impl<Req, St: 'static> RequestState<Req> for State<St, Req> {
    type State = St;

    #[inline]
    fn unpack(self) -> (St, Req) {
        let State { state, req } = self;
        (state, req)
    }
}

impl<Req, St: 'static> RequestState<Req> for (St, Req) {
    type State = St;

    #[inline]
    fn unpack(self) -> (St, Req) {
        (self.0, self.1)
    }
}
