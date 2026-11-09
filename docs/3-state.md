# Service State

Services often need information beyond the request they are handling. This
might be connection metadata, a database pool, application configuration, or a
channel to another subsystem.

Putting these dependencies into every request would mix long-lived resources
with the data for one operation. Storing a copy in every service can work, but
then each wrapper has to carry dependencies it does not itself use. Pipeline
state gives the service graph one place to obtain this shared context.

The `St` type parameter on `Service` represents this information:

```rust,ignore
trait Service<St, Req> {
    type Res;
    type Error;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

A service borrows pipeline state through [`Ctx`]. It can still own fields,
such as configuration or inner services. You can access pipeline state during
calls, readiness checks, and shutdown:

```rust,ignore
async fn call(&self, req: Req, ctx: Ctx<'_, Self, AppState>) -> Result<Self::Res, Self::Error> {
    let state: &AppState = ctx.st();
    // ...
}
```

Rust checks that the pipeline supplies a compatible state type because that
type is part of `Service<St, Req>`. You cannot place a service that requires
`AppState` directly in a pipeline with an unrelated state type.

Keep the three kinds of data separate. The request is the input consumed by
one call. A service's fields belong to that service instance: its configuration,
children, or a cache used only by that component. `St` is the context supplied
to the service graph, so several components can borrow the same database pool
or connection metadata without embedding it in each request.

For example, `Service<AppState, String>` processes a `String` using `AppState`;
`Service<(), String>` has no additional state. The order matters: `St` comes
first. It is an ordinary Rust type, not a string-keyed container, so dependency
requirements appear in trait bounds and the compiler checks them. Low-level
service state does not require the web framework's `web::State` trait.

## State-aware functions

You can turn an asynchronous function into a state-aware service by making
`&St` its first argument:

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

The function receives `&AppState` on each call. The [`IntoService`]
implementation converts it into a service automatically.

Use [`fn_service_st`] explicitly if you need to name the service value or pass
it through an API before creating the pipeline. Use `fn_service` instead when
the function only takes a request; it does not receive pipeline state.

The state is borrowed, not cloned for every call. `Pipeline::new` owns both the
state and service, and requires them to be `'static`. An owned `String` or `Rc`
can meet that bound; `'static` does not mean you must leak memory or use a global
variable. A reference to a local stack value generally cannot be stored there.
The state type itself need not implement `Clone` for `Pipeline::new`.

## Testing state-aware function services

You can test a state-aware function service without starting a server. Create
the service with [`fn_service_st`]. Put it and the test state in a pipeline
using [`Pipeline::new`], then call it with the request value.

This exercises the same state access and readiness handling used in production.

If the state contains external dependencies, you can use a mocking library to
check how the service uses them. These examples show two approaches:

- [`tests/mockall`] defines the state behavior as a trait. Mockall creates a
  mock state and checks calls to its accessor and operation methods.
- [`tests/shimforge`] keeps the concrete state type. Shimforge replaces methods
  during the test, including a method on an object nested in the state. It
  checks the receiver's attributes and call arguments.

Both examples configure call counts and ordering. Each invokes the service
through a pipeline and asserts its response.

## Pipeline-owned state

Use [`Pipeline::new`] to keep one service and one state value together. Every
call, readiness check, and shutdown operation uses that same state instance.

The state belongs to the pipeline, rather than the process. Each new pipeline
owns the state value you give it. Whether the underlying data is independent
or shared depends on that value: two separately constructed counters are
independent, but two `Rc` clones point to the same counter. Similarly, several
worker-local states can each hold a clone of the same `Arc<Pool>`.

Pipeline bindings share the original pipeline and its state. Cloning a
[`PipelineBinding`] does not clone the state value. Bindings also participate
in the pipeline's coordinated readiness and shutdown handling.

### Sharing Mutable State

A borrowed state value can still contain mutable data. For worker-local
counters, `Cell` inside an `Rc` is enough. Here a separate handle lets the
caller observe how many requests the service processed:

```rust
use std::{cell::Cell, convert::Infallible, rc::Rc};

use ntex::{Pipeline, service::fn_service_st};

struct Counters {
    calls: Rc<Cell<usize>>,
}

async fn record_call(state: &Counters, value: usize) -> Result<usize, Infallible> {
    state.calls.set(state.calls.get() + 1);
    Ok(value)
}

#[ntex::main]
async fn main() {
    let calls = Rc::new(Cell::new(0));
    let pipeline = Pipeline::new(
        Counters { calls: calls.clone() },
        fn_service_st(record_call),
    );
    let handle = pipeline.bind();

    assert_eq!(pipeline.call(10).await.unwrap(), 10);
    assert_eq!(handle.call(20).await.unwrap(), 20);
    assert_eq!(calls.get(), 2);
}
```

Both calls reach the same service and counter. Creating a binding does not
construct another `Counters` value. You also do not need to make `Counters`
implement `Clone` just to share this pipeline.

Pipelines and their bindings are thread-local. Use `Rc`, `Cell`, and `RefCell`
for data confined to that thread; use `Arc` with suitable synchronization for
resources deliberately shared across workers. An `Arc` alone does not make
mutable data thread-safe. When using `RefCell`, end a mutable borrow before an
`.await` that could let another request borrow the same value.

## Calling nested services

When your service wraps another service, call the child through `Ctx`. The
context provides `ctx.st()` and connects the call to the pipeline's readiness
and shutdown handling:

```rust,ignore
async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
    ctx.call(&self.inner, req).await
}
```

[`Ctx::call`] reuses valid readiness from the nearest upstream scope. On a
cache miss, it checks only the target service. A child wrapped with `ready(svc)`
also checks its own independent cache.

Calls do not consume cached success. When the cache is valid, the call adds no
readiness allocations. Use `ctx.call(&self.inner, req).await` even after an
explicit readiness check. It keeps this fast path while observing any
invalidation between the check and the call.

`Ctx` also provides `ready()` and `shutdown()` operations. Wrappers must forward
the lifecycle methods to their children:

- `Service::bind`
- `Service::ready`
- `Service::shutdown`

You can use `forward!` to forward these methods. A service whose readiness
changes must subscribe to a `ReadyNotifier` in `bind` and notify on every
relevant change.

A raw `Ctx::ready` call does not populate an upstream cache. Use `ready(svc)`
when a child needs independent, persistent readiness.

Readiness checks and their wake sources must run on the pipeline's thread.

A child receives the same `&St` as its parent unless you explicitly substitute
state. A generic wrapper that does not read it can implement `Service<St, Req>`
for any compatible `St`; it should not hard-code `()` and accidentally exclude
stateful children. The `ResponseHeaderService` middleware in
[Writing a Middleware Service](./8.3-web-app.md#writing-a-middleware-service)
shows the complete pattern with `forward!`.

If constructing a child needs state, its factory receives `&St` too. A factory
can use that reference to configure a new service, but a service stored in an
owned pipeline cannot retain a short-lived borrow of the factory argument.
Clone an owned resource handle into the service when it needs to keep one, or
borrow the state through `Ctx` on each call. For a lazily created child, use
`ctx.create(&factory).await` so creation also registers its dependencies.

## Substituting state

Use [`map_state`] when one service in a chain needs a different state type. It
wraps that service with a fixed state value. The outer pipeline can keep using
its own state type:

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

The adapter owns `Metrics` and supplies it to the inner service's readiness,
call, and shutdown methods. It does not convert the outer state or look up a
field in it. Use this for a component with fixed private context, not as a way
to select a different tenant or user on each request. The surrounding readiness
scope is preserved; replacing state does not create an independent capacity
gate.

## State supplied per operation

Use [`PipelineState`] when callers need to select the state for each
operation. It stores a service without owning its state. The caller supplies a
state reference for each readiness check, call, or shutdown operation:

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

Use `Pipeline` when one state value should stay attached to the service.
`PipelineState` keeps the same service and readiness machinery while letting
each caller choose the state.

To attach an owned state value, call `PipelineState::bind_state()`. It produces
a normal `PipelineBinding` and requires the state type to implement `Clone`.

All of these handles share one readiness cache and pending checker, even when
they supply different state values. Passing a different state does not
invalidate cached success.

`Service::ready` may inspect the state, but its result must describe the shared
pipeline-wide gate. Use separate pipelines or service bindings if you need
independent, state-specific gates. For example, changing a tenant identifier
between two calls is not enough to re-check a tenant-specific quota. A shared
cache could still contain the first tenant's success. Keep such admission
checks in request processing or give independently limited services their own
gates.

## Carrying state with a request

At some protocol boundaries, the input contains both a request and the state
for a nested pipeline. The [`RequestState`] trait represents this input.
This packaging does not itself change any pipeline's state: the receiving
service unpacks it and supplies the state to the nested pipeline.

[`State<St, Req>`] implements the trait by splitting the input into a state and
a request. The tuple `(St, Req)` implements it too:

```rust
use ntex::service::{RequestState, State};

let input = State {
    state: "connection-1",
    req: "request",
};

let (connection, request) = input.unpack();
assert_eq!(connection, "connection-1");
assert_eq!(request, "request");
```

For example, ntex HTTP services use `RequestState` to extract an accepted
connection's state. They provide that state to the HTTP request and
control-service pipelines. Connection setup state stays separate from the HTTP
request value and keeps its concrete type.

A plain `Io` also implements `RequestState`, using `()` as its state. An
ordinary server can therefore pass accepted connections to `HttpService`
without wrapping them. See
[Passing Connection State to the HTTP Service](6-server-app.md#passing-connection-state-to-the-http-service)
for an example that wraps an accepted `Io` in `State`.

## Choosing State at Application Boundaries

Start with `Pipeline` when the service and its context should have the same
lifetime. Use `PipelineState` when one service must work with a caller-selected
context. Use `map_state` for a fixed inner context, and `State` or a tuple to
carry context across a protocol boundary. These choices control who supplies
the value; none automatically creates a process-wide shared resource.

The [server state chapter](./6-server-app.md#worker-local-and-process-wide-state)
applies the same distinction to workers and connections. Server configuration
creates worker state; connection setup can add metadata before passing an
accepted stream to HTTP. Expensive pools and caches usually belong to worker
or process setup, not to an application service constructed for each
connection.

For web handlers, [Web Application State](./8.2-web-app.md) builds on this model
with the `web::State` trait and handler APIs. Its application state is distinct
from request-local state carried through web filters. Keeping those lifetimes
separate avoids two common surprises: expecting a plain cloned value to share
mutations across connections, and putting one request's identity into a
long-lived worker-wide value.

[`Ctx`]: https://docs.ntex.rs/api/ntex/struct.Ctx.html
[`Ctx::call`]: https://docs.ntex.rs/api/ntex/struct.Ctx.html#method.call
[`fn_service_st`]: https://docs.ntex.rs/api/ntex/service/fn.fn_service_st.html
[`IntoService`]: https://docs.ntex.rs/api/ntex/trait.IntoService.html
[`map_state`]: https://docs.ntex.rs/api/ntex/service/fn.map_state.html
[`Pipeline::new`]: https://docs.ntex.rs/api/ntex/struct.Pipeline.html#method.new
[`PipelineBinding`]: https://docs.ntex.rs/api/ntex/struct.PipelineBinding.html
[`PipelineState`]: https://docs.ntex.rs/api/ntex/service/pipeline/struct.PipelineState.html
[`RequestState`]: https://docs.ntex.rs/api/ntex/service/trait.RequestState.html
[`State<St, Req>`]: https://docs.ntex.rs/api/ntex/service/struct.State.html
[`tests/mockall`]: https://github.com/ntex-rs/examples/tree/main/tests/mockall
[`tests/shimforge`]: https://github.com/ntex-rs/examples/tree/main/tests/shimforge
