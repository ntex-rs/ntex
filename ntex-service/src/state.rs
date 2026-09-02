pub trait RequestState<Res> {
    type State: 'static;

    fn unpack(self) -> (Res, Self::State);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State<Req, St> {
    pub req: Req,
    pub state: St,
}

impl<Req, St: 'static> RequestState<Req> for State<Req, St> {
    type State = St;

    #[inline]
    fn unpack(self) -> (Req, St) {
        let State { req, state } = self;
        (req, state)
    }
}
