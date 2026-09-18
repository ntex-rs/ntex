# Service and Pipeline State

Services often need access to information that is not part of an individual
request. Examples include connection metadata, a database pool, application
configuration, or a channel to another subsystem.

The `Service` trait represents this information with its `St` type parameter:

```rust,ignore
trait Service<St, Req> {
    type Res;
    type Error;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

A service borrows state through [`Ctx`]; it does not own the state itself.
State is available during calls, readiness checks, and shutdown:

```rust,ignore
async fn call(&self, req: Req, ctx: Ctx<'_, Self, AppState>) -> Result<Self::Res, Self::Error> {
    let state: &AppState = ctx.st();
    // ...
}
```

Because the state type is part of `Service<St, Req>`, Rust verifies that a
service is used with compatible state. A service requiring `AppState` cannot be
placed directly in a pipeline that supplies an unrelated type.

## State-aware functions

An asynchronous function whose first argument is `&St` can be converted into a
state-aware service. This is the simplest way to define one:

```rust
use std::convert::Infallible;

use ntex::Pipeline;

struct AppState {
    prefix: &'static str,
}

async fn format_value(state: &AppState, value: usize) -> Result<String, Infallible> {
    Ok(format!("{}-{value}", state.prefix))
}

#[ntex::main]
async fn main() {
    let service = Pipeline::new(AppState { prefix: "item" }, format_value);

    assert_eq!(service.call(10).await.unwrap(), "item-10");
}
```

The function receives `&AppState` for each call. ntex performs this conversion
through [`IntoService`] trait implementation; for this case it uses [`fn_service_st`] type,
calling it explicitly is useful when the service value must be named or
passed through an API before creating the pipeline.

## Pipeline-owned state

[`Pipeline::new`] stores one service and one state value together. Every call,
readiness check, and shutdown operation on that pipeline uses the same state
instance.

The state belongs to that pipeline rather than to the process. Creating another
pipeline creates another state instance unless the application explicitly
shares a resource. For example, several worker-local states can each hold a
clone of the same `Arc<Pool>`.

Pipeline bindings share the original pipeline and its state; cloning a
[`PipelineBinding`] does not clone the state value. Bindings also participate
in the pipeline's coordinated readiness and shutdown handling.

## Calling nested services

`Ctx` does more than provide `ctx.st()`. It connects a service call to the
pipeline's readiness and shutdown machinery. A service that wraps another
service should call it through the context:

```rust,ignore
async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
    ctx.call(&self.inner, req).await
}
```

[`Ctx::call`] waits for the inner service to become ready before calling it.
[`Ctx::call_nowait`] skips that check and should only be used when readiness has
already been established. `Ctx` also provides corresponding `ready()` and
`shutdown()` operations for implementing service combinators and middleware.

Calling an inner service's trait methods directly bypasses this pipeline
coordination is not possible.

## Substituting state

Sometimes one service in a larger chain needs a different state type.
[`map_state`] wraps that service with a fixed state value while allowing the
outer pipeline to use another state type:

```rust
use std::convert::Infallible;

use ntex::{Pipeline, service::map_state};

struct Metrics {
    prefix: &'static str,
}

async fn record(metrics: &Metrics, value: usize) -> Result<String, Infallible> {
    Ok(format!("{}-{value}", metrics.prefix))
}

#[ntex::main]
async fn main() {
    let service = map_state(Metrics { prefix: "metric" }, record);

    // The wrapped service uses Metrics even though the outer pipeline uses ().
    let pipeline = Pipeline::new((), service);
    assert_eq!(pipeline.call(10).await.unwrap(), "metric-10");
}
```

## State supplied per operation

[`PipelineState`] stores a service without owning its state. The caller supplies
a state reference for each readiness check, call, or shutdown operation:

```rust
use std::convert::Infallible;

use ntex::service::pipeline::PipelineState;

struct RequestContext {
    prefix: &'static str,
}

async fn format_value(state: &RequestContext, value: usize) -> Result<String, Infallible> {
    Ok(format!("{}-{value}", state.prefix))
}

#[ntex::main]
async fn main() {
    let service = PipelineState::new(format_value);

    let first = RequestContext { prefix: "first" };
    let second = RequestContext { prefix: "second" };

    assert_eq!(service.call(1, &first).await.unwrap(), "first-1");
    assert_eq!(service.call(2, &second).await.unwrap(), "second-2");
}
```

Use `Pipeline` when one state value should remain attached to the service. Use
`PipelineState` when the same service and readiness machinery must operate with
a state selected by each caller. `PipelineState::bind_state()` can attach an
owned state value and produce a normal `PipelineBinding`.

## Carrying state with a request

Some protocol boundaries receive both a request and the state that a nested
pipeline should use. The [`RequestState`] trait represents such an input.
[`State<St, Req>`] and `(St, Req)` implement it by splitting the value into a
state and a request:

```rust
use ntex::service::State;

let input = State {
    state: connection_state,
    req: io,
};
```

For example, ntex HTTP services use `RequestState` to extract state associated
with an accepted connection and then provide that state to the HTTP request and
control-service pipelines. This keeps connection setup state separate from the
HTTP request value while preserving its concrete type.

[`Ctx`]: https://docs.rs/ntex/latest/ntex/struct.Ctx.html
[`Ctx::call`]: https://docs.rs/ntex/latest/ntex/struct.Ctx.html#method.call
[`Ctx::call_nowait`]: https://docs.rs/ntex/latest/ntex/struct.Ctx.html#method.call_nowait
[`Ctx::map_state`]: https://docs.rs/ntex/latest/ntex/struct.Ctx.html#method.map_state
[`fn_service_st`]: https://docs.rs/ntex/latest/ntex/fn.fn_service_st.html
[`map_state`]: https://docs.rs/ntex/latest/ntex/service/fn.map_state.html
[`map_state_factory`]: https://docs.rs/ntex/latest/ntex/service/fn.map_state_factory.html
[`Pipeline::new`]: https://docs.rs/ntex/latest/ntex/struct.Pipeline.html#method.new
[`PipelineBinding`]: https://docs.rs/ntex/latest/ntex/struct.PipelineBinding.html
[`PipelineState`]: https://docs.rs/ntex/latest/ntex/service/pipeline/struct.PipelineState.html
[`RequestState`]: https://docs.rs/ntex/latest/ntex/service/trait.RequestState.html
[`State<St, Req>`]: https://docs.rs/ntex/latest/ntex/service/struct.State.html
