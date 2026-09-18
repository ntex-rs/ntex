# Component and Service Model

The term *component model* can make a simple idea sound more complicated than it
really is. It often brings to mind IoC containers, layers of dependency
injection, and plenty of indirection. That is not what we are trying to build.

Our goal is much simpler: reusable components that are easy to understand and,
most importantly, easy to combine.

Imagine a service that receives an operation, performs some work, and returns a
result. Its implementation might be quite complex, but callers should not need
to know about any of that. From the outside, the service should have a small,
predictable interface.

In Rust, the most natural way to express this is with a function:

```rust
async fn execute(op: Operation) -> Result<OperationResult, Error> {
    // Execute the operation.
    // ...
}
```

That is all the caller needs to see. Give the function an `Operation`, and it
either returns an `OperationResult` or reports an `Error`. How it produces that
result is an implementation detail.

This small abstraction already gives us almost everything we need: one input,
one output, and a clear contract. There is no hidden framework behavior or
unnecessary ceremony.

It is also easy to understand, straightforward to test, and naturally
composable.

## Building an HTTP Endpoint

Suppose we want to expose our execution service through an HTTP endpoint. What
else do we need?

At a minimum, we need a thin layer to handle the HTTP-specific details. It must
deserialize the incoming request, call the execution service, and serialize the
result into an HTTP response.

Once again, the shape can stay simple. The endpoint can be just another
function:

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

There is nothing particularly fancy happening here. The endpoint is simply
glue code:

1. Turn the HTTP request into a domain value—an `Operation`.
2. Pass the operation to the execution service.
3. Turn the resulting `OperationResult` into an HTTP response.

Each part has a single responsibility, and the boundaries are explicit. The
execution engine knows nothing about HTTP, while the HTTP layer does not need to
know how the operation is executed.

More importantly, these pieces do not depend on one another directly. Each one
simply transforms one type into another.

That is the key idea.

## Composing Services

To compose two services, the output of one simply needs to match the input of
the next. When the types line up, the services naturally form a transformation
chain.

In this example, the overall transformation is from an HTTP request to an HTTP
response:

```text
HttpRequest -> HttpResponse
```

Everything that happens in between is an implementation detail.

We can add more steps, such as authentication and authorization, without
changing the overall shape of the system:

```rust
async fn endpoint(req: HttpRequest) -> Result<HttpResponse, Error> {
    // Authenticate the request.
    let req = authenticate(req).await?;

    // Extract the operation from the request.
    let operation = load_operation(req).await?;

    // Check whether the operation is allowed.
    let operation = authorize(operation).await?;

    // Execute the operation.
    let result = execute(operation).await?;

    // Convert the result into an HTTP response.
    into_response(result).await
}
```

The processing flow now looks like this:

```text
HttpRequest
    -> authenticate
    -> load operation
    -> authorize
    -> execute
    -> build response
    -> HttpResponse
```

Each step is small and focused. It receives a value, performs its work, and
returns the value expected by the next step. There is no tight coupling between
the components.

Once we start thinking in these terms, the component model becomes almost
trivial: components are functions, and composition means connecting compatible
outputs and inputs.

Despite its simplicity, this model scales surprisingly far.

## Defining a Service

Now that we have the basic idea, we can formalize what a *service* actually is.

At its core, a service accepts an input and produces an output. The operation
may be asynchronous, and it may fail. That should already sound familiar—it is
the same shape as the functions in the previous examples.

We can describe this idea with a small `Service` trait that resembles Rust's
`Fn` traits:

```rust
trait Service<Req> {
    /// Response produced by the service.
    type Res;

    /// Error produced by the service.
    type Error;

    async fn call(&self, req: Req) -> Result<Self::Res, Self::Error>;
}
```

This trait gives us a common way to describe any transformation from a request
to a response.

The real value lies in what this common interface allows us to build. We are no
longer limited to ordinary functions. A service can be:

- an asynchronous function;
- a struct with its own configuration or internal state;
- a wrapper around another service; or
- a chain composed of several services.

As long as every component follows the same `Service` contract, we can write
generic tools that work with all of them. This is what makes middleware,
combinators, and reusable pipelines possible.

Instead of calling a collection of unrelated functions, we now have a system in
which every component is composable by design.

## Services in Networking

This model is not limited to HTTP endpoints. If you look closely at networking
code—especially the generic, reusable parts—you will find the same pattern
almost everywhere.

Consider a TCP connection handler:

```rust
impl Service<TcpStream, Res = (), Error = io::Error>
```

It receives a `TcpStream` and either handles the connection successfully or
returns an error. Once connection handling follows this interface, we can build
a generic server that works with any compatible TCP service. This is the basic
idea behind [ntex-server](https://crates.io/crates/ntex-server).

A TCP connector fits the same model:

```rust
impl Service<net::SocketAddr, Res = TcpStream, Error = io::Error>
```

It receives a socket address and returns an established TCP connection. From
the caller's point of view, it is simply another transformation:

```text
SocketAddr -> TcpStream
```

A TLS handshake is another example:

```rust
impl<T: Stream> Service<T, Res = TlsStream<T>, Error = io::Error>
```

It receives a plain stream and returns a TLS-enabled stream:

```text
T -> TlsStream<T>
```

The TLS service does not need to know whether the stream came from a server, a
client, or an in-memory transport. It only needs a compatible stream as input.

All these components follow the same basic pattern:

```text
input -> service -> output
```

The types differ, but the model remains the same. A connector creates a stream,
a TLS service transforms it, and a connection handler consumes it. Because
these components share a common interface, they can be wrapped, reused, and
composed without needing to understand one another's implementation.

This is the same idea we explored with HTTP requests, applied to the rest of the
networking stack.

## From a Simple Trait to a Practical Framework

The [ntex-service](https://github.com/ntex-rs/ntex/tree/main/ntex-service) crate
turns this model into a practical abstraction for real applications. The entire
[ntex framework](https://github.com/ntex-rs/ntex) builds on it, from low-level
connection handling to high-level HTTP and web services.

Of course, a production-ready service model needs more than the minimal
`Service` trait shown here. It must also support concerns such as:

- service initialization and configuration;
- readiness and backpressure;
- graceful shutdown;
- middleware;
- service composition; and
- pipeline state.

These features add some complexity, but they do not change the core idea. A
service still accepts an input and produces an output. Everything else exists
to make that simple model reliable, reusable, and practical at scale.
