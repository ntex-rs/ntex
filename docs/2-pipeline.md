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

### Readiness Between Services

Readiness must also be respected while a request moves through the service
chain. A service should not invoke the next service directly, because doing so
would bypass that service's readiness check.

Instead, services call one another through an execution context:

```rust
let result = ctx.call(&next_service, request).await?;
```

The context waits for `next_service` to become ready before invoking it. This
allows backpressure to propagate through the chain, even if readiness changes
while a request is being processed.

In ntex-service, this execution context is represented by `Ctx`. It carries the
pipeline-level information needed to coordinate otherwise independent service
instances.

A useful mental model is:

- [`Service`](https://docs.rs/ntex-service/latest/ntex_service/trait.Service.html)
  defines what happens at each step.
- `ServiceChain` describes how the steps are connected.
- [`Pipeline`](https://docs.rs/ntex-service/latest/ntex_service/struct.Pipeline.html)
  owns the runnable graph and coordinates shared readiness.
- [`Ctx`](https://docs.rs/ntex-service/latest/ntex_service/struct.Ctx.html)
  controls how execution moves safely between services.

## The Complete `Service` Trait

Once we include state, readiness, and shutdown, a simplified version of the ntex
`Service` trait looks like this:

```rust
trait Service<St, Req> {
    type Res;
    type Error;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error>;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

The context serves several related purposes:

- It provides access to the pipeline state.
- It connects services to the pipeline that owns them.
- It coordinates readiness between services.
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
