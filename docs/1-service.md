# Component and Service Model

ntex is built from small pieces called *services*. A service takes a request,
does some work, and eventually returns either a response or an error. The
request is whatever input the service expects: an HTTP request, a TCP
connection, a decoded MQTT packet, or a value from your own domain. Almost
everything in ntex, from accepting connections to routing web requests, is a
service or is built from services.

This chapter explains the idea and the reasoning behind it. The exact API comes
in the next chapters.

## Start with a Function

Imagine a component that receives an operation, performs some work, and returns
a result. Its implementation might be complex, but callers should not need to
know about any of that. From the outside, it should have a small, predictable
interface.

In Rust, the most natural way to express this is an async function:

```rust,ignore
async fn execute(op: Operation) -> Result<OperationResult, Error> {
    // Execute the operation.
    // ...
}
```

That is all the caller needs to see. Give the function an `Operation`, and it
either returns an `OperationResult` or reports an `Error`. How it produces that
result is an implementation detail. One input, one output, and a clear
contract: the function is easy to understand, easy to test, and easy to
combine with others.

## Building an HTTP Endpoint

Suppose we want to expose `execute` through an HTTP endpoint. We need a thin
layer for the HTTP details: read an operation from the request, run it, and
turn the result into a response. That layer can be another function:

```rust,ignore
async fn endpoint(req: HttpRequest) -> Result<HttpResponse, Error> {
    // Extract an operation from the request.
    let operation = load_operation(req).await?;

    // Execute the operation.
    let result = execute(operation).await?;

    // Convert the result into an HTTP response.
    into_response(result).await
}
```

The endpoint is only glue. The execution code knows nothing about HTTP, and the
HTTP layer does not know how operations are executed. Neither depends on the
other directly; each step turns one type into another.

## Composing Services

To connect two steps, the output of one only has to match the input of the
next. That makes it easy to add steps, such as authentication and
authorization, without changing the overall shape:

```rust,ignore
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

From the outside, it is still one transformation from `HttpRequest` to
`HttpResponse`. Everything in between is an implementation detail, and each
step can be written, tested, and replaced on its own.

## Services in Networking

The same pattern shows up all over networking code, especially in the generic,
reusable parts.

A TCP connection handler receives a `TcpStream` and either handles the
connection or fails with an I/O error:

```text
TcpStream -> ()
```

Once connection handling looks like this, a generic server can run any
compatible connection service. That is the basic idea behind
[ntex-server](https://crates.io/crates/ntex-server).

A TCP connector receives a socket address and returns an established
connection:

```text
SocketAddr -> TcpStream
```

A TLS handshake receives a plain stream and returns an encrypted one:

```text
T -> TlsStream<T>
```

The TLS service does not need to know whether the stream came from a server, a
client, or an in-memory transport. It only needs a stream it can work with.

The types differ, but the shape stays the same: input, service, output. A
connector creates a stream, a TLS service transforms it, and a connection
handler consumes it. Because they share one model, these pieces can be wrapped,
reused, and combined without knowing how the others work.

## More Than a Function

Treating all of these the same way pays off quickly. An async function or a
closure can be a service. So can a struct that carries configuration, a
connection pool, or a counter. A chain of services joined end to end is itself
a service, so it can be wrapped, chained again, or handed to a server. Any
tool written for "a service" works with every one of them.

A service can also hold another service and add behavior around it; that is
all middleware is. The wrapper does not call its inner service directly. It
goes through a small context that ntex passes to every call, which keeps the
whole chain working as one unit.

A plain function is a good mental model, but network code needs a bit more.
The ntex service model adds a few ideas on top of "input in, output out". None
of them changes the basic shape. Each one answers a practical question that
comes up as soon as real traffic arrives.

### One Service, Many Requests

A service handles requests through a shared reference. While one request waits
on a database or a socket, the same service can start working on the next one.
A connection handler, for example, serves every connection its worker accepts,
all at the same time.

ntex runs each worker on its own thread, and each worker gets its own instance
of the service. A service never moves between threads, so it can keep mutable
state in a `Cell` or `RefCell` instead of behind a lock. The reasons for this
design are covered [below](#why-one-thread-per-worker).

### Ready Before the Call

A function cannot say "not now". A service can. Before a request is handed
over, the caller waits until the service reports that it is ready. A service at
its connection limit, or one whose database pool is used up, stays not ready
until capacity frees up. The request waits with the caller instead of piling up
in a hidden queue inside the service.

This is how ntex handles backpressure. You rarely check readiness by hand: when
services are chained, the chain only accepts a request once every service in it
is ready, so a slow step at the end holds back new work at the front. Readiness
can also fail, which tells the caller that the service cannot accept requests
at all.

The [readiness example](https://github.com/ntex-rs/examples/tree/main/service/readiness)
shows both cases with console output: a concurrency limit that holds back new
work and a gate whose readiness fails while it is closed.

### Services Come from Factories

Services often need some setup before the first request. They may need to read
configuration, open a connection, or warm a cache. That work can be
asynchronous, and it can fail. ntex keeps it out of request handling by using a
*service factory*, an object whose only job is to build a service.

A server keeps the factory and asks it for a new service on every worker. If
setup fails, the factory reports an initialization error, separate from the
errors a service returns while handling requests. You will often pass a factory
where you might expect a service; an async function works as either one.

### Shutting Down

Services also take part in an orderly shutdown. When a worker stops, it asks
its service to shut down and waits, up to a timeout, for it to finish. A
service can use that time to flush buffers, close connections, or let work in
progress complete. A wrapper shuts down the services it holds as well.

## What It Looks Like in Code

Here is the model in real code. The first example turns two async closures into
services and joins them. The first service parses a number from a string, and
the second doubles it:

```rust
use ntex::service::{Pipeline, fn_service, service};

#[ntex::main]
async fn main() -> Result<(), String> {
    let parse = fn_service(async |line: String| {
        line.trim().parse::<u32>().map_err(|e| e.to_string())
    });
    let double = fn_service(async |n: u32| Ok::<_, String>(n * 2));

    // The chain is itself a service. A `Pipeline` is how you call it.
    let pipeline = Pipeline::new((), service(parse).and_then(double));

    assert_eq!(pipeline.call(" 21 ".to_string()).await?, 42);
    assert!(pipeline.call("abc".to_string()).await.is_err());
    Ok(())
}
```

The chain takes a `String` and returns a `u32`. If parsing fails, `double`
never runs and the error goes straight back to the caller. The `()` passed to
`Pipeline::new` is the pipeline's state. This example does not need any.

The second example uses a factory. It reads a limit from the configuration once,
when the service is built, and the service then checks every request against
it:

```rust
use std::convert::Infallible;

use ntex::service::{factory, fn_factory, fn_service};

struct Limits {
    max: u32,
}

#[ntex::main]
async fn main() -> Result<(), String> {
    let check = fn_factory(async |limits: &Limits| {
        let max = limits.max;
        Ok::<_, Infallible>(fn_service(async move |n: u32| {
            if n <= max { Ok(n) } else { Err(format!("{n} is over {max}")) }
        }))
    });

    let pipeline = factory(check).pipeline(Limits { max: 100 }).await.unwrap();

    assert_eq!(pipeline.call(42).await?, 42);
    assert!(pipeline.call(500).await.is_err());
    Ok(())
}
```

Setup can't fail here, so the factory's error type is `Infallible`. A factory
that opens a database connection would return a real error, reported when the
service is built, before it handles any request.

Closures are enough for small steps. Larger services are usually structs that
implement the `Service` trait directly, which is shown in the next chapter.

## Why One Thread per Worker

Many async runtimes move tasks between threads to keep every CPU busy. That
works well, but it has a cost: anything a task holds must be safe to send to
another thread. Shared data ends up behind `Arc`, `Mutex`, or atomics, and the
hot path pays for synchronization even when there is no real contention.

ntex takes a different approach. A server starts several workers, usually one
per CPU core. Each worker is a thread with its own single-threaded runtime and
its own instance of every service. A separate accept thread spreads new
connections across the workers, and a connection stays on the worker that
accepted it for its whole life.

Because nothing moves between threads, services and their futures do not need
to be `Send`. That has practical benefits:

* State can use `Rc`, `Cell`, and `RefCell`. Updating a counter or a small
  cache is a plain memory write, not a lock or an atomic operation.
* A worker's data stays local to that thread, and to its CPU's cache when
  workers are pinned to cores.
* Code is easier to reason about. Requests on one worker interleave only at
  `.await` points, never in the middle of a statement.

The model has trade-offs too:

* Work cannot move to an idle worker. If one connection does a lot of
  CPU-heavy work, other connections on that worker wait. Run blocking or
  long computations on the blocking thread pool with
  [`spawn_blocking()`](./4-runtime.md#blocking-work).
* Data shared by all workers must be shared explicitly, with `Arc` and
  thread-safe types. Often it is simpler for each worker to build its own copy
  in the service factory. This is why the factory itself must be `Send`,
  while the services it creates need not be.
* Requests interleave at `.await` points, so don't hold a `RefCell` borrow
  across an `.await`. Another request on the same worker may try to borrow it.

For network servers, where most time is spent waiting on sockets and many
small requests run at once, this usually gives better throughput and more
predictable latency. The [Server](./5-server.md) chapter covers worker counts,
CPU placement, and load balancing.

## From an Idea to a Framework

The [ntex-service](https://github.com/ntex-rs/ntex/tree/main/ntex-service)
crate turns this model into code, and the rest of
[ntex](https://github.com/ntex-rs/ntex) is built on it.

The next two chapters make the ideas above concrete.
[Service Pipelines](./2-pipeline.md) shows how services are combined, how
readiness moves through a chain, and what the full `Service` trait looks like.
[Service State](./3-state.md) covers the state a pipeline shares with its
services. After that, you can go to the [Server](./5-server.md) or the web
framework, starting with [Routing and Handlers](./8.1-web-app.md), and you will
find the same model underneath.

## Glossary

These terms come up throughout the guide.

* **Service**: something that takes a request and, asynchronously, returns a
  response or an error. An async function or closure, or a struct that
  implements the `Service` trait.
* **Request** and **response**: a service's input and output. They can be any
  Rust type: an HTTP request, a TCP stream, a number.
* **Service factory**: an object that builds a service. Building can be async
  and can fail.
* **Initialization error**: the error a factory returns when it can't build a
  service. It is separate from the errors a service returns while handling
  requests.
* **Chain**: services joined end to end, so the output of one becomes the input
  of the next. A chain is itself a service.
* **Middleware**: a service that wraps another service and adds behavior around
  its calls, such as logging, timeouts, or extra headers.
* **Pipeline**: a service together with its state, ready to be called. It also
  keeps readiness consistent across the services inside it.
* **State**: data a pipeline shares with all of its services, such as a
  database pool or application settings. See [Service State](./3-state.md).
* **Context**: a small value ntex passes to every call. A service uses it to
  reach the pipeline state and to call its inner services.
* **Readiness**: a service's signal that it can accept another request. A
  service that is not ready makes callers wait.
* **Backpressure**: slowing down the producers of work when the consumers can't
  keep up. In ntex, readiness provides it.
* **Worker**: a thread with its own runtime and its own instances of the
  application's services.
* **Shutdown**: the step where a service finishes outstanding work and releases
  resources before its worker stops.
