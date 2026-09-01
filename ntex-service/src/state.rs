use std::marker::PhantomData;

use crate::{Ctx, IntoService, Service};

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

#[derive(Clone, Debug)]
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

impl<St, Req, Builder> StateBuilder<St, Req, Builder>
where
    Builder: StateBuilderStep<St, Req, InState = ()>,
{
    /// Build state builder service
    pub fn build(
        self,
    ) -> impl Service<St, Req, Res = State<Builder::Res, Builder::OutState>, Error = Builder::Error>
    {
        self
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
    pub fn and_then<S>(
        self,
        svc: impl IntoService<S, St, Builder::Res>,
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
                    svc: svc.into_service(),
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

#[derive(Clone, Debug)]
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

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, St>) {}
}

#[derive(Clone, Debug)]
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
        (): (),
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(Self::Res, Self::OutState), Self::Error> {
        (self.0)(ctx.st(), &req).await.map(move |st| (req, st))
    }
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
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
            .and_then(Svc1)
            .map(async |_: &(), r: &usize, st: St| Ok(St { n: st.n + *r }))
            .and_then(Svc2);

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

// SAFETY: Send cannot be provided authomatically because of St param
// but code get executed in one thread and never leave it
