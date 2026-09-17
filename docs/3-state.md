# Service and Pipeline State

Sometimes several services need access to the same information. For example, we
might collect connection details while accepting a connection and make them
available to HTTP handlers later. Services may also need to share long-lived
resources such as a database connection pool or a channel to a backend system.

The `Service` trait supports this through its generic `St` parameter. `St`
describes the type of state a service expects:

```rust
trait Service<St, Req> {
    type Res;
    type Error;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error>;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error>;

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>);
}
```

A service does not own this state. Instead, the pipeline owns it and makes it
available to the services it contains. Every compatible service in the
pipeline can access the same state while checking readiness, processing a
request, or shutting down.

Because `St` is part of the `Service` definition, Rust can verify that a service
is used with the correct state type. A service that expects `AppState`, for
example, cannot be placed in a pipeline that provides an unrelated type.

The state is available throughout the service lifecycle:

- `ready()` can use it while checking whether the service can accept more work.
- `call()` can use it while processing a request.
- `shutdown()` can use it while the service is stopping.

Conceptually, a stateful service looks like an asynchronous function that
receives both the state and the request:

```rust
async fn my_service(state: &St, req: Req) -> Result<Res, Error> {
    // ...
}
```

The `Service` trait does not pass state as a separate argument. Instead, it
provides access through `Ctx`:

```rust
async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
    let state = ctx.st();

    // Process the request using the pipeline state.
    // ...
}
```

`Ctx` does more than expose the state. It connects the service to its pipeline,
allowing the pipeline to coordinate readiness, calls, and shutdown across the
entire service chain.

Passing state from one protocol layer to another requires a little more
coordination. For example, `HttpService` can extract state produced while
accepting a connection and make it available to HTTP request handlers. Other
ntex protocol implementations, including HTTP/2, MQTT, and AMQP, follow the
same general model. We will look at this in more detail in the following
sections.

## Constructing a Pipeline

Before a service can be called, it must be placed in a `Pipeline`. The pipeline
keeps the service and its state together. It can contain either a single service
or a chain of services, all sharing the same compatible state.

Here is a small example:

```rust
async fn my_service(state: &AppState, req: usize) -> Result<String, io::Error> {
    // Process the request using the pipeline state.
    todo!()
}

#[ntex::main]
async fn main() -> io::Result<()> {
    // Create a pipeline with one state instance and one service.
    let svc = ntex::Pipeline::new(AppState::new(), my_service);

    // Both calls use the same AppState instance.
    let first = svc.call(1).await?;
    let second = svc.call(2).await?;

    println!("{first}:{second}");

    Ok(())
}
```

`AppState` is provided when the pipeline is created and remains associated with
it for the pipeline's entire lifetime. Every call uses the same state instance;
calling `svc.call()` does not create a new one.

If the pipeline contains a service chain, every compatible service in the chain
can access the same state. This lets related services share resources without
storing or passing them explicitly at every step.

Pipeline state is local to that pipeline—it is not automatically global to the
process. If multiple pipelines or worker threads need access to the same
resource, it must be shared explicitly. One common approach is to store an
`Arc` in each pipeline's state.

A complete working example is available in the
[ntex examples repository](https://github.com/ntex-rs/examples/tree/main/state5).
