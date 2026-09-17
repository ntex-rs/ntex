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

This function is really a sequence of fallible, asynchronous transformations.
Each step takes the output of the previous step and either produces the next
value or returns an error.

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
error, the remaining stages are skipped and the error is returned by the
pipeline.

The completed pipeline is itself a service:

```rust
Service<HttpRequest, Res = HttpResponse>
```

From the outside, it behaves just like the original `endpoint` function. It
accepts an `HttpRequest` and produces an `HttpResponse`, while the intermediate
steps remain an implementation detail.

The advantage of this approach is that each stage remains independent. A
service can be replaced, removed, or inserted without rewriting the rest of the
pipeline.

For example, adding request throttling is a local change:

```rust
let pipeline = service(authenticate)
    .and_then(throttle)  // <- new stage in pipeline, throttling
    .and_then(load_operation)
    .and_then(authorize)
    .and_then(execute)
    .and_then(into_response);
```

The only requirement is that the types line up: the output of one service must
match the input expected by the next. But external inteface hast changed,
it is still transformation from `HttpRequest -> HttpResponse`.

A throttling service can be designed for a specific request type:

```rust
Service<HttpRequest, Res = HttpRequest>
```

Such a service inspects an HTTP request and returns it unchanged when the
request is allowed to continue.

It can also be generic over its input:

```rust
Service<R, Res = R>
```

A generic throttling service acts as a reusable concurrency or rate-limiting
boundary. It can wrap any stage of a pipeline without depending on HTTP or other
domain-specific types.

This is what makes service pipelines useful: they let us build larger behavior
from small, focused transformations while preserving a simple interface for the
result.

==============

Given the `Service` trait, we can describe `and_then` as a generic operation.
It accepts two services and connects them so that the output of the first
becomes the input of the second:

```rust
fn and_then<Req, A, B>(
    first: A,
    second: B,
) -> impl Service<Req, Res = B::Res, Error = A::Error>
where
    A: Service<Req>,
    B: Service<A::Res, Error = A::Error>,
{
    AndThen { first, second }
}
```

The type constraints express the rules of composition:

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

    async fn call(
        &self,
        req: Req,
    ) -> Result<Self::Res, Self::Error> {
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
[`ServiceChain`](https://docs.rs/ntex-service/latest/ntex_service/dev/struct.ServiceChain.html)
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

A `Pipeline` adds an important runtime guarantee: readiness. Before dispatching
a request, it makes sure that every service involved in processing that request
is ready to accept work. If one of them is at capacity, processing waits until
the service becomes ready again.

This distinction is useful:

- `ServiceChain` describes how services are composed.
- `Pipeline` owns the runnable service graph and coordinates its readiness,
  calls, state, and shutdown.

The composition remains fully typed. If one service produces a value that the
next service cannot accept, the chain fails to compile. This catches invalid
pipelines while they are being assembled rather than after the application is
running.

==========================

## Readiness

So far, `Service::call()` describes what a service does, but it says nothing
about when the service is able to accept more work.

Real services are not always ready. A service may have reached its limit for
in-flight requests, filled an internal queue, or be waiting for an external
resource such as a connection from a pool. If callers can invoke `call()` at any
time, the service must either buffer an unlimited amount of work or implement
backpressure through some separate, ad hoc mechanism.

We can make backpressure part of the service contract by adding a readiness
check:

```rust
trait Service<Req> {
    type Res;
    type Error;

    async fn ready(&self) -> Result<(), Self::Error>;

    async fn call(
        &self,
        req: Req,
    ) -> Result<Self::Res, Self::Error>;
}
```

Conceptually, `ready()` acts as a gate. It waits until the service can accept
more work without exceeding its internal limits. If the service cannot become
ready—for example, because an underlying resource has failed—it returns an
error.

A caller therefore follows this pattern:

```rust
service.ready().await?;
let response = service.call(request).await?;
```

For a composed service such as `AndThen<A, B>`, readiness is no longer a local
property. A request may pass through both services, so the combined service is
ready only when both components are ready.

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

    async fn call(
        &self,
        req: Req,
    ) -> Result<Self::Res, Self::Error> {
        let intermediate = self.first.call(req).await?;
        self.second.call(intermediate).await
    }
}
```

This captures the basic rule: the composed service should not accept a request
unless every stage needed to process it is ready.

A production implementation can check both services concurrently rather than
waiting for them one at a time. This matters when both components are under
load, because progress in either service may be needed before the full chain
becomes ready.

Readiness also needs to be coordinated across concurrent callers. A readiness
check is not merely a convenience method attached to an individual service; it
is part of the behavior of the whole service graph. This is one of the main
reasons ntex services are called through a `Pipeline`.

The pipeline tracks readiness for the composed service and waits until the
required services can accept work before dispatching a request. Backpressure
therefore flows through the same abstractions as request processing instead of
being handled separately by each caller.

=============================================

## Pipeline Readiness

Unlike `call()`, readiness is not entirely local to one service. In a composed
service, the pipeline must consider every service that may participate in
processing the request.

This matters because execution is asynchronous and several requests may be in
flight at the same time. Whether the pipeline can accept more work may depend on
the combined load of its inner services: active calls, queue capacity,
connection limits, or external resources.

Readiness is therefore a cross-cutting concern. It cannot be managed reliably
by looking at each call in isolation.

The `Pipeline` coordinates readiness across the complete service graph. Before
dispatching a request, it waits until the services required to process that
request are ready. If an inner service reaches capacity, execution pauses until
that service can accept more work.

### Shared Readiness

Concurrent calls share the readiness state of the same underlying pipeline.
Each active call receives its own pipeline binding, which identifies it while
readiness is being coordinated.

A normal call creates this binding internally:

```rust
let response = pipeline.call(request).await?;
```

When a call needs to be stored, moved into another task, or allowed to outlive
the borrow of `pipeline`, `Pipeline::call_static()` returns an owned future that
keeps its binding alive:

```rust
let call = pipeline.call_static(request);
let response = call.await?;
```

Both forms use the same underlying service graph and shared readiness state.
The difference is in how the lifetime of the call is managed.

This coordination prevents concurrent callers from independently driving the
same readiness check. It also avoids hiding excess work in unbounded internal
queues: when the pipeline is not ready, callers wait.

### Readiness Between Services

Readiness must also be respected as a request moves through a service chain. A
service should not call the next service directly because doing so would bypass
the next service's readiness check.

Instead, services call one another through an execution context:

```rust
let result = ctx.call(&next_service, request).await?;
```

The context waits for `next_service` to become ready before invoking it. This
allows backpressure to propagate through the chain, even when readiness changes
while a request is being processed.

In ntex-service, this context is represented by `Ctx`. It carries the
pipeline-level information needed to coordinate readiness across otherwise
independent service instances.

A useful mental model is:

- [`Service`](https://docs.rs/ntex-service/latest/ntex_service/trait.Service.html)
  defines what happens at each step.
- `ServiceChain` describes how those steps are connected.
- [`Pipeline`](https://docs.rs/ntex-service/latest/ntex_service/struct.Pipeline.html)
  owns the runnable service graph and coordinates its shared readiness.
- [`Ctx`](https://docs.rs/ntex-service/latest/ntex_service/struct.Ctx.html)
  controls how execution moves safely between services.

### The Complete `Service` Trait

With state, readiness, and shutdown included, a simplified version of the ntex
`Service` trait looks like this:

```rust
trait Service<St, Req> {
    type Res;
    type Error;

    async fn ready(
        &self,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<(), Self::Error>;

    async fn call(
        &self,
        req: Req,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error>;

    async fn shutdown(
        &self,
        ctx: Ctx<'_, Self, St>,
    );
}
```

The context serves several related purposes:

- It provides access to the pipeline state.
- It links services to the pipeline that owns them.
- It coordinates readiness between services.
- It allows one service to call another without bypassing backpressure.
- It participates in orderly service shutdown.

The important point is that readiness is not merely a method that callers are
expected to use correctly. It is built into the way requests move through a
pipeline. This makes backpressure explicit, composable, and enforceable without
relying on hidden queues or coordination outside the service model.

### `shutdown()`

The `shutdown()` method represents the final stage of the service lifecycle:

```rust
async fn shutdown(&self);
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
