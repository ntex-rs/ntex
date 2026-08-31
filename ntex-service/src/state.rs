#![allow(dead_code, unused_variables, missing_debug_implementations)]

use std::marker::PhantomData;

use crate::{Ctx, Service};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State<Req, St> {
    pub req: Req,
    pub state: St,
}

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

    fn map(&self, req: Self::Req) -> (Self::Res, Self::State);
}

#[derive(Debug)]
pub struct DefaultState<Req>(PhantomData<Req>);

impl<Req> DefaultState<Req> {
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<Req> Copy for DefaultState<Req> {}

impl<Req> Clone for DefaultState<Req> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}

impl<Req> RequestState for DefaultState<Req> {
    type Req = Req;
    type Res = Req;
    type State = ();

    fn map(&self, req: Req) -> (Self::Req, Self::State) {
        (req, ())
    }
}

// SAFETY: Send cannot be provided authomatically because of St param
// but code get executed in one thread and never leave it
unsafe impl<Req> Send for DefaultState<Req> {}
unsafe impl<St> Send for CloneState<St> {}
unsafe impl<F, St> Send for FnState<F, St> where F: Send {}

pub struct StateBuilder<St, Req, Builder = ()> {
    step: Builder,
    t: PhantomData<(St, Req)>,
}

impl<St, Req> StateBuilder<St, Req> {
    pub fn new<F, State, Err>(f: F) -> StateBuilder<St, Req, StateBuilderInit<F>>
    where
        F: AsyncFn(&St, &Req) -> Result<State, Err>,
    {
        StateBuilder {
            step: StateBuilderInit(f),
            t: PhantomData,
        }
    }
}

impl<St, Req, Builder> Service<St, Req> for StateBuilder<St, Req, Builder>
where
    Builder: StateBuilderStep<St, Req, InState = ()>,
{
    type Res = State<Builder::Res, Builder::OutState>;
    type Error = Builder::Error;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        let (req, state) = self.step.call(req, (), Ctx::new(idx, waiters, st)).await?;
        Ok(State { req, state })
    }
}

impl<St, Req, Builder> StateBuilder<St, Req, Builder> {
    pub fn service<S>(
        self,
        svc: S,
    ) -> StateBuilder<
        St,
        Req,
        StateBuilderStack<St, Req, Builder, StateBuilderService<S, Builder::OutState>>,
    >
    where
        S: Service<St, Builder::Res>,
        S::Error: From<Builder::Error>,
        Builder: StateBuilderStep<St, Req>,
    {
        StateBuilder {
            step: StateBuilderStack {
                inner: self.step,
                outer: StateBuilderService {
                    svc,
                    st: PhantomData,
                },
                st: PhantomData,
            },
            t: PhantomData,
        }
    }

    pub fn map<F, NewState, Error>(
        self,
        f: F,
    ) -> StateBuilder<
        St,
        Req,
        StateBuilderStack<St, Req, Builder, StateBuilderMapState<F, Builder::OutState>>,
    >
    where
        F: AsyncFn(&St, &Builder::Res, Builder::OutState) -> Result<NewState, Error>,
        Error: From<Builder::Error>,
        Builder: StateBuilderStep<St, Req>,
    {
        StateBuilder {
            step: StateBuilderStack {
                inner: self.step,
                outer: StateBuilderMapState { f, st: PhantomData },
                st: PhantomData,
            },
            t: PhantomData,
        }
    }
}

pub struct StateBuilderStack<St, Req, Inner, Outer> {
    inner: Inner,
    outer: Outer,
    st: PhantomData<(St, Req)>,
}

impl<St, Req, State, Inner, Outer> StateBuilderStep<St, Req>
    for StateBuilderStack<St, Req, Inner, Outer>
where
    Inner: StateBuilderStep<St, Req, InState = State>,
    Outer: StateBuilderStep<St, Inner::Res, InState = Inner::OutState>,
    Outer::Error: From<Inner::Error>,
{
    type Res = Outer::Res;
    type Error = Outer::Error;

    type InState = Inner::InState;
    type OutState = Outer::OutState;

    async fn call(
        &self,
        req: Req,
        state: State,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Self::Res, Self::OutState), Outer::Error> {
        let (idx, waiters, st) = ctx.inner();
        let (res, state) = self
            .inner
            .call(req, state, Ctx::new(idx, waiters, st))
            .await?;
        self.outer
            .call(res, state, Ctx::new(idx, waiters, st))
            .await
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let (idx, waiters, st) = ctx.inner();
        self.inner.ready(Ctx::new(idx, waiters, st)).await?;
        self.outer.ready(Ctx::new(idx, waiters, st)).await?;
        Ok(())
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        let (idx, waiters, st) = ctx.inner();
        self.inner.shutdown(Ctx::new(idx, waiters, st)).await;
        self.outer.shutdown(Ctx::new(idx, waiters, st)).await;
    }
}

pub trait StateBuilderStep<St, Req> {
    type Res;
    type InState;
    type OutState;
    type Error;

    async fn call(
        &self,
        req: Req,
        st: Self::InState,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Self::Res, Self::OutState), Self::Error>;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {}
}

pub struct StateBuilderInit<F>(F);

impl<F, St, Req, State, Err> StateBuilderStep<St, Req> for StateBuilderInit<F>
where
    F: AsyncFn(&St, &Req) -> Result<State, Err>,
{
    type Res = Req;
    type InState = ();
    type OutState = State;
    type Error = Err;

    async fn call(
        &self,
        req: Req,
        st: (),
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Self::Res, Self::OutState), Self::Error> {
        (self.0)(ctx.st(), &req).await.map(move |st| (req, st))
    }
}

pub struct StateBuilderService<S, State> {
    svc: S,
    st: PhantomData<State>,
}

impl<S, St, Req, State> StateBuilderStep<St, Req> for StateBuilderService<S, State>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type InState = State;
    type OutState = State;
    type Error = S::Error;

    async fn call(
        &self,
        req: Req,
        st: State,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Self::Res, Self::OutState), Self::Error> {
        ctx.call(&self.svc, req).await.map(move |res| (res, st))
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.svc).await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&self.svc).await;
    }
}

pub struct StateBuilderMapState<F, State> {
    f: F,
    st: PhantomData<State>,
}

impl<F, St, Req, State, NewState, Err> StateBuilderStep<St, Req> for StateBuilderMapState<F, State>
where
    F: AsyncFn(&St, &Req, State) -> Result<NewState, Err>,
{
    type Res = Req;
    type InState = State;
    type OutState = NewState;
    type Error = Err;

    async fn call(
        &self,
        req: Req,
        st: State,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Req, NewState), Err> {
        (self.f)(ctx.st(), &req, st).await.map(move |st| (req, st))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ctx, Pipeline, Service};

    #[derive(Debug, PartialEq, Eq)]
    struct St {
        n: usize,
    }

    #[derive(Debug, Clone)]
    struct Svc1;

    impl Service<(), &'static str> for Svc1 {
        type Res = usize;
        type Error = ();

        async fn call(&self, _: &'static str, _: Ctx<'_, Self>) -> Result<Self::Res, ()> {
            Ok(22)
        }
    }

    #[derive(Debug, Clone)]
    struct Svc2;

    impl Service<(), usize> for Svc2 {
        type Res = &'static str;
        type Error = ();

        async fn call(&self, req: usize, _: Ctx<'_, Self>) -> Result<Self::Res, ()> {
            Ok("test")
        }
    }

    #[ntex::test]
    async fn test_state_builder() {
        let sb = StateBuilder::new(async |_: &(), t: &&'static str| Ok::<_, ()>(St { n: t.len() }))
            .service(Svc1)
            .map(async |_: &(), r: &usize, st: St| Ok(St { n: st.n + *r }))
            .service(Svc2);

        let pl = Pipeline::with((), sb);
        let res = pl.call("test").await.unwrap();
        assert_eq!(
            res,
            State {
                req: "test",
                state: St { n: 26 }
            }
        );
    }
}
