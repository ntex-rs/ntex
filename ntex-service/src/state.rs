//! Requests that carry state for a service call.
//!
//! [`RequestState`] splits an input into `(state, request)`. Both the named
//! [`State<St, Req>`] wrapper and the tuple `(St, Req)` implement it, allowing
//! protocol boundaries to accept either representation.
//!
//! These types package data; they do not create a pipeline or automatically
//! change its state. After unpacking, the caller can pass the state to a
//! [`crate::pipeline::PipelineState`] operation or use it to construct a
//! [`crate::Pipeline`]. A service accesses the supplied state through
//! [`crate::Ctx::st`].
//!
//! # Example
//!
//! ```
//! use ntex_service::state::{RequestState, State};
//!
//! let input = State {
//!     state: "connection-1",
//!     req: 42,
//! };
//! let (state, request) = input.unpack();
//! assert_eq!(state, "connection-1");
//! assert_eq!(request, 42);
//!
//! assert_eq!(("connection-2", 7).unpack(), ("connection-2", 7));
//! ```
//!
//! # Framework guide
//!
//! See [Carrying state with a request](https://docs.ntex.rs/3-state.html#carrying-state-with-a-request).

/// A request that carries state for a service call.
pub trait RequestState<Req> {
    /// State extracted from the request.
    type State: 'static;

    /// Splits this value into its state and request components.
    fn unpack(self) -> (Self::State, Req);
}

/// A request paired with state for the duration of a service call.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State<St, Req> {
    /// Request passed to the service.
    pub req: Req,
    /// State made available while processing the request.
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
