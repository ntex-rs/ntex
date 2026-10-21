# Service Pipelines

In the previous section, we described what a `Service` is and how to define one.
Consider the HTTP endpoint from our earlier example:

```rust
async fn endpoint(req: HttpRequest) -> Result<HttpResponse, Error> {
    // Extract an operation from the request.
    let operation = load_operation(req).await?;

    // Execute the operation.
    let result = execute(operation).await?;

    // Convert the result into an HTTP response.
    into_response(result).await
}
```

This function is a sequence of fallible, asynchronous transformations. Each
step takes the output of the previous step and either produces the next value or
returns an error.

That structure maps naturally to a service pipeline. We can compose the same
steps with an `and_then` combinator, similar to `Result::and_then()`:

```rust
let pipeline = service(authenticate)
    .and_then(load_operation)
    .and_then(authorize)
    .and_then(execute)
    .and_then(into_response);
```

Each service runs only if the previous one succeeds. If any stage returns an
error, the remaining stages are skipped and the pipeline returns that error.

The completed pipeline is itself a service:

```rust
Service<HttpRequest, Res = HttpResponse>
```

From the outside, it behaves just like the original `endpoint` function. It
accepts an `HttpRequest` and produces an `HttpResponse`. Everything that happens
between those two types remains an implementation detail.

The benefit of this approach is that each stage remains independent. We can
replace, remove, or insert a service without rewriting the rest of the
pipeline.

For example, adding request throttling is a local change:

```rust
let pipeline = service(authenticate)
    .and_then(throttle) // <- throttling service
    .and_then(load_operation)
    .and_then(authorize)
    .and_then(execute)
    .and_then(into_response);
```

The external interface has not changed. The pipeline still represents the same
overall transformation:

```text
HttpRequest -> HttpResponse
```

The only requirement is that the types line up: the output of one service must
match the input expected by the next.

A throttling service can be designed specifically for HTTP requests:

```rust
Service<HttpRequest, Res = HttpRequest>
```

Such a service inspects a request and returns it unchanged when the request is
allowed to continue.

The service can also be generic over its input:

```rust
Service<R, Res = R>
```

A generic throttling service can act as a reusable concurrency or rate-limiting
boundary anywhere in a pipeline. It does not need to know anything about HTTP
or the application's domain types.

This is what makes service pipelines useful: they let us build larger behavior
from small, focused transformations while preserving a simple interface for the
result.

## Implementing `and_then`

Given the `Service` trait, we can describe `and_then` as a generic operation. It
accepts two services and connects them so that the output of the first becomes
the input of the second:

```rust
fn and_then<Req, A, B>(first: A, second: B) -> impl Service<Req, Res = B::Res, Error = A::Error>
where
    A: Service<Req>,
    B: Service<A::Res, Error = A::Error>,
{
    AndThen { first, second }
}
```

The type constraints describe the rules of composition:

- `first` accepts the original request.
- `second` accepts the response produced by `first`.
- Both services use the same error type.
- The combined service returns the response produced by `second`.

The implementation is equally straightforward:

```rust
struct AndThen<A, B> {
    first: A,
    second: B,
}

impl<Req, A, B> Service<Req> for AndThen<A, B>
where
    A: Service<Req>,
    B: Service<A::Res, Error = A::Error>,
{
    type Res = B::Res;
    type Error = A::Error;

    async fn call(&self, req: Req) -> Result<Self::Res, Self::Error> {
        let intermediate = self.first.call(req).await?;
        self.second.call(intermediate).await
    }
}
```

The first service processes the request and produces an intermediate value. If
it succeeds, that value is passed to the second service. If it fails, the `?`
operator returns the error immediately and the second service is never called.

This is ordinary sequential composition with short-circuiting on error. Other
familiar combinators, such as `then`, `map`, and `map_err`, follow the same
general pattern.

In practice, this machinery is already available in the
[ntex-service](https://github.com/ntex-rs/ntex/tree/main/ntex-service) crate.
Its
[`ServiceChain`](https://docs.ntex.rs/api/ntex_service/dev/struct.ServiceChain.html)
type provides a fluent, strongly typed API for composing services:

```rust
let chain = service(authenticate)
    .and_then(load_operation)
    .and_then(authorize)
    .and_then(execute)
    .and_then(into_response);
```

Each call wraps the existing chain in another service combinator. The resulting
`ServiceChain` is itself a service, so it can be extended further, wrapped in
middleware, or placed in a `Pipeline`.

The entire chain remains strongly typed. If one service produces a value that
the next service cannot accept, the code fails to compile. Invalid pipelines
are therefore caught while the application is being built rather than after it
starts running.

## Readiness and Backpressure

So far, `Service::call()` describes what a service does, but it says nothing
about when the service is able to accept more work.

Real services are not always ready. A service may have reached its limit for
in-flight requests, filled an internal queue, or be waiting for an external
resource such as a database connection.

If callers can invoke `call()` at any time, the service must either buffer an
unlimited amount of work or implement backpressure through a separate,
non-composable mechanism.

We can make backpressure part of the service contract by adding a readiness
check:

```rust
trait Service<Req> {
    type Res;
    type Error;

    async fn ready(&self) -> Result<(), Self::Error>;

    async fn call(&self, req: Req) -> Result<Self::Res, Self::Error>;
}
```

Conceptually, `ready()` acts as a gate. It waits until the service can accept
more work without exceeding its internal limits. If the service cannot become
ready—for example, because an underlying resource has failed—it returns an
error.

A caller would follow this pattern:

```rust
service.ready().await?;
let response = service.call(request).await?;
```

For a composed service such as `AndThen<A, B>`, readiness is no longer local to
one component. A request may pass through both services, so the combined service
is ready only when both components are ready.

A simplified implementation looks like this:

```rust
impl<Req, A, B> Service<Req> for AndThen<A, B>
where
    A: Service<Req>,
    B: Service<A::Res, Error = A::Error>,
{
    type Res = B::Res;
    type Error = A::Error;

    async fn ready(&self) -> Result<(), Self::Error> {
        self.first.ready().await?;
        self.second.ready().await?;
        Ok(())
    }

    async fn call(&self, req: Req) -> Result<Self::Res, Self::Error> {
        let intermediate = self.first.call(req).await?;
        self.second.call(intermediate).await
    }
}
```

This captures the basic rule: the composed service should not accept a request
unless every stage needed to process it is ready.

A production implementation can check the services concurrently rather than
waiting for them one at a time. More importantly, it must coordinate readiness
across all concurrent calls to the same service graph.

That coordination is the responsibility of the `Pipeline`.

## Pipeline Readiness

A `Pipeline` turns a service chain into a runnable service graph. It owns the
graph's state and coordinates readiness, calls, and shutdown.

This matters because several requests may be in flight at the same time.
Whether the pipeline can accept more work may depend on active calls, queue
capacity, connection limits, or external resources used by any of its inner
services.

Readiness is therefore a cross-cutting concern. It cannot be managed reliably
by looking at each call or service in isolation.

Before dispatching a request, the pipeline waits until the required services
are ready. If one of them reaches capacity, processing pauses until that service
can accept more work.

This gives us a useful distinction:

- `ServiceChain` describes how services are connected.
- `Pipeline` owns the runnable service graph and coordinates its lifecycle.

### Shared Readiness

Concurrent calls use the same underlying pipeline and therefore share its
readiness state. Each active call receives its own pipeline binding, which
identifies the call while readiness is being coordinated.

A normal call creates this binding internally:

```rust
let response = pipeline.call(request).await?;
```

Sometimes a call must be stored, moved into another task, or allowed to outlive
the borrow of `pipeline`. In that case, `Pipeline::call_static()` returns an
owned future that keeps its binding alive:

```rust
let call = pipeline.call_static(request);
let response = call.await?;
```

Both methods use the same service graph and shared readiness state. The
difference is how the lifetime of the call is managed.

This coordination prevents multiple callers from independently driving the same
readiness check. It also avoids hiding excess work in unbounded internal queues:
when the pipeline is not ready, callers wait.

The pipeline owns a readiness scope. A successful readiness check, such as
`Pipeline::poll_ready()`, is cached by that scope until a service notifies it
that its readiness has changed. Every binding and every call of the pipeline
reuses the cached result, and calls do not consume it. A dispatcher that waits
for readiness before each call therefore re-checks the services only after
something has changed.

### The Service Tree

A composed pipeline is a directed tree of services. The pipeline is the root,
combinators are inner nodes, and the services that do the actual work are the
leaves:

```rust
let pipeline = service(authenticate)
    .and_then(ready(throttle))
    .and_then(execute)
    .pipeline(());
```

```text
Pipeline                        <- scope
└── AndThen
    ├── AndThen
    │   ├── authenticate
    │   └── ready(throttle)     <- scope
    └── execute
```

Requests flow down the tree, from the root to the leaves. Readiness flows in
the opposite direction: every node depends on the readiness of the nodes below
it, so readiness links point backward from the leaves to the root. The pipeline
is ready only when `authenticate`, `throttle`, and `execute` are all ready.

A service shared through `Rc` can be bound under several parents. Its readiness
then feeds every one of them, which turns the tree into a directed acyclic
graph.

#### Building Dependencies in `Service::bind()`

Readiness links are not discovered at runtime. Each service declares them once
in `Service::bind()`. The pipeline calls `bind()` when it is constructed, and
`Ctx::create()` calls it for services created later. In both cases, `bind()`
runs synchronously before any `ready()`, `call()`, or `shutdown()`.

The [`BindCtx`](https://docs.ntex.rs/api/ntex_service/struct.BindCtx.html)
passed to `bind()` registers dependencies in the current readiness scope:

- `ctx.subscribe(&notifier)` registers a `ReadyNotifier` that the service uses
  to report changes in its own readiness.
- `ctx.bind(&child)` binds a child service. It must be called for every child
  the service uses, including children that are only called and never checked
  for readiness. A raw child shares the current scope. A `ServiceBinding` or
  `ready()` child links its own scope below the current one.
- `ctx.on_notify(&callback)` optionally registers a `ReadyCallback` that runs
  when a notification reaches the current scope.
- A child pipeline is bound with `pipeline.bind_to(&ctx)`.

```rust
struct Limited<S> {
    inner: S,
    changed: ReadyNotifier,
}

impl<S: Service<(), u8>> Service<(), u8> for Limited<S> {
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.changed); // changes of our own readiness
        ctx.bind(&self.inner);        // readiness of the child
    }

    // ready() and call() ...
}
```

When a child implements `Service` for several request types, pass the types
explicitly, for example `ctx.bind::<_, St, Req>(&self.first)`. Services that
only forward to their children can use the `forward_bind!(St, field, ...)` or
`forward_pl_bind!` macros. The default `bind()` does nothing, which is correct
for leaf services that have no notification sources.

A few rules follow from this design:

- All dependencies must be known at bind time. `ready()` and `call()` cannot
  register new ones.
- Using a `ServiceBinding` that its parent did not bind panics, so a missing
  `bind()` forward is detected instead of silently losing notifications.
- Shared services can be bound more than once. Repeated subscriptions are
  idempotent.
- Children created lazily at runtime must use `ctx.create(&factory).await`,
  which binds the new service in the current scope and invalidates it.

### Readiness Scopes

A readiness scope caches a successful readiness check of its subtree until it
is notified that readiness has changed. Not every node of the tree has its own
scope. Scopes exist only at:

- the pipeline root,
- each [`ServiceBinding`](https://docs.ntex.rs/api/ntex_service/struct.ServiceBinding.html),
- each service wrapped with `ready()` or created by `ready_factory()`.

Plain services and combinators such as `and_then`, `then`, and `apply` store
their children raw, so those children share the nearest scope above them.

A scope's cached readiness remains valid only while every service below it is
still ready:

- When a service calls `ReadyNotifier::notify()`, the scope it is subscribed to
  and every scope above it are invalidated before any waiting task is woken.
  Scopes in unrelated branches stay valid. In the example above, a notification
  from `throttle` invalidates both scopes, while one from `execute` invalidates
  only the pipeline scope.
- A notification never marks a service ready. It only forces the next `ready()`
  or `call()` to check again.

A service whose readiness can change must therefore notify on every change, such
as taking the last free slot, releasing a slot, a timer firing, or an external
failure, before another caller can reuse the stale result. Waking a task is not
the same as invalidating a scope: a wake only re-polls a pending check.

```rust
fn acquire(&self) {
    self.in_flight.set(self.in_flight.get() + 1);
    self.changed.notify(); // may have become not ready
}

fn release(&self) {
    self.in_flight.set(self.in_flight.get() - 1);
    self.changed.notify(); // may have become ready
}
```

### Observing Readiness Notifications

Use a reusable
[`ReadyCallback`](https://docs.ntex.rs/api/ntex_service/struct.ReadyCallback.html)
to observe explicit readiness notifications. A service registers its handle
during `Service::bind`:

```rust
fn bind(&self, ctx: BindCtx<'_>) {
    ctx.subscribe(&self.changed);
    ctx.on_notify(&self.on_notify);
    ctx.bind(&self.inner);
}
```

Pipelines also expose `pipeline.on_notify(&callback)`, as do `PipelineBinding`,
`PipelineState`, and `PipelineStateBinding`. For example:

```rust
use ntex::service::ReadyCallback;

let callback = ReadyCallback::new(|| println!("readiness notified"));
# let pipeline = ntex::Pipeline::new((), async |_: ()| Ok::<_, ()>(()));
pipeline.on_notify(&callback);
pipeline.bind().on_notify(&callback); // Same scope and identity: one registration.
```

The callback runs synchronously when `ReadyNotifier::notify()` reaches its
scope, including notifications propagated from child scopes and repeated
notifications while the scope is already invalid. It does not run for
successful readiness checks, cached calls, ordinary task wakeups, or internal
resets such as lazy service creation and shutdown. All affected scopes are
invalidated before callbacks run, outside internal borrows and locks.

Nested notifications still invalidate every affected scope, but skip callback
delivery on scopes already executing callbacks. Registrations made during that
delivery are retained for later notifications. Delivery reuses each scope's
registered callback storage without allocating a snapshot or cloning handles.

Callbacks are registered on the nearest scope: raw services share their
enclosing one, while `ready(svc)` gives the service its own. Registering one
handle on two affected scopes invokes it once for each scope. Cloning a handle
preserves its identity. Registrations persist until the scope is dropped,
independently of readiness checks and runtime binding handles. Callbacks must
own their captures; avoid strong reference cycles through a pipeline that
retains the callback.

### Readiness Between Services

Readiness must also be respected while a request moves through the service
chain. A service should not invoke the next service directly, because doing so
would bypass that service's readiness check.

Instead, services call one another through an execution context:

```rust
let result = ctx.call(&next_service, request).await?;
```

In ntex-service, this execution context is represented by `Ctx`. It carries the
pipeline-level information needed to coordinate otherwise independent service
instances, including the nearest readiness scope above the current service.

`Ctx` uses that scope to decide whether a readiness check is needed:

| Operation | Behavior |
|---|---|
| `ctx.call(&svc, req)`, scope valid | Dispatches immediately, without a check. |
| `ctx.call(&svc, req)`, scope invalid | Waits for `svc.ready()` only. The scope above is not marked ready. |
| `ctx.call(&binding, req)` | Checks the binding's own scope, independent of the scope above. |
| `ctx.ready(&svc)` | Checks the raw service every time and does not update any scope. |
| `ctx.ready(&binding)` | Reuses the binding's scope, re-checking only after a notification. |

The same rules apply to services wrapped with `ready()`, which use a
`ServiceBinding` internally. This allows backpressure to propagate through the
chain, even if readiness changes while a request is being processed.

Use `ready()` when a child needs its own readiness, independent of its siblings.
A typical case is a second stage called after an `.await`: its readiness may
change in the meantime, and with its own scope it is re-checked only when it
notifies, not every time a sibling changes.

A useful mental model is:

- [`Service`](https://docs.ntex.rs/api/ntex_service/trait.Service.html)
  defines what happens at each step.
- `ServiceChain` describes how the steps are connected.
- [`Pipeline`](https://docs.ntex.rs/api/ntex_service/pipeline/struct.Pipeline.html)
  owns the runnable graph and its root readiness scope.
- `ServiceBinding` and `ready()` add independent readiness scopes inside the
  graph.
- [`Ctx`](https://docs.ntex.rs/api/ntex_service/struct.Ctx.html)
  controls how execution moves safely between services.

## The Complete `Service` Trait

Once we include state, readiness, and shutdown, a simplified version of the ntex
`Service` trait looks like this:

```rust
trait Service<St, Req> {
    type Res;
    type Error;

    fn bind(&self, ctx: BindCtx<'_>) {}

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error>;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

The context serves several related purposes:

- It provides access to the pipeline state.
- It connects services to the pipeline that owns them.
- It coordinates readiness between services through the nearest readiness
  scope.
- It allows one service to call another without bypassing backpressure.
- It participates in orderly shutdown.

The important point is that readiness is not merely a method that callers are
expected to use correctly. It is built into the way requests move through a
pipeline. This makes backpressure explicit, composable, and enforceable without
relying on hidden queues or coordination outside the service model.

The `St` parameter represents the service state made available through the
context. We will explore service and pipeline state in the next section.

## Shutdown

The `shutdown()` method represents the final stage of the service lifecycle:

```rust
async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
```

It is called by the service's owner—typically a `Pipeline`—when the service is
being taken out of operation.

A service can use `shutdown()` to:

- stop background tasks gracefully;
- release external resources;
- flush buffered data; and
- allow in-flight work to finish cleanly.

Simple services may not need to do anything during shutdown. Services that own
resources or run background tasks, however, should use this method to clean up
properly rather than stopping abruptly.

Composite services should also propagate shutdown to their inner services so
that the entire pipeline stops cleanly.
