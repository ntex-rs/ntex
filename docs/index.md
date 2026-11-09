# ntex framework guide

ntex is a Rust framework for building asynchronous network services. You can
use it for an HTTP API, a web application, or a server that speaks your own
protocol. Its web framework handles routing, request extraction, middleware,
and responses, while the lower-level crates let you work directly with
connections and messages. You can start with the web APIs without learning
all of those layers first.

This guide is for Rust developers who want to build something with ntex and
understand how its pieces fit together. It assumes you know Rust's basic
types, traits, and `async`/`await`; it does not assume experience with ntex.
Examples show the public APIs, and the linked API reference gives the full
type signatures and options when you need them.

The common building block is a service: a component that accepts a request
and produces a response or an error asynchronously. A request might be an
HTTP request, a decoded protocol message, or a connection. Pipelines give
services a shared context for state, readiness, calls, and shutdown. That
makes the same composition model useful for a small handler and for a stack
of protocol services. You will meet services gradually, rather than needing
to implement one before your first web route.

ntex also separates protocol code from the runtime that drives it. You can
choose Tokio, Compio, or ntex's native runtime without rewriting your web
handlers. The I/O layer provides buffered connections and filters, so concerns
such as TLS can sit below your protocol implementation. These layers matter
most when you need a custom protocol or control over connection behavior;
ordinary web applications usually let the HTTP server assemble them.

Servers run applications on workers. Each worker can initialize its own state,
such as a client handle or cache, and that state can use single-threaded
shared types. When data must be shared across workers, you choose a
thread-safe shared handle instead. The distinction helps you decide where
initialization belongs and what a state clone should share, rather than
treating every application value as process-wide.

Start with [Getting Started](./getting-started.md) to run a server, add a few
routes, and write a test. If you are building a web application, continue
with chapter 8, beginning with [Routing and Handlers](./8.1-web-app.md). You
can return to the service and server chapters when you need more control over
startup, state, or middleware.

If you are writing a protocol, begin with [Service Model](./1-service.md),
[Service Pipelines](./2-pipeline.md), and the [I/O Abstraction
Layer](./7-io.md). The chapters are also useful independently: follow the
links for the layer you are working on, or use the cookbook for a focused
example. The API reference, LLM documentation, and migration guide are
listed below the chapters.

- [Getting Started](./getting-started.md)
- [Service Model](./1-service.md)
- [Service Pipelines](./2-pipeline.md)
- [Service State](./3-state.md)
- [Runtime](./4-runtime.md)
- [Server](./5-server.md)
- [Worker and Request State](./6-server-app.md)
- [I/O Abstraction Layer](./7-io.md)
- Web Application Framework
  - [Routing and Handlers](./8.1-web-app.md)
  - [Application State](./8.2-web-app.md)
  - [Filters and Middleware](./8.3-web-app.md)
  - [Extractors and Responders](./8.4-web-app.md)
  - [Error Handling](./8.5-web-app.md)
  - [WebSocket Support](./8.6-web-app.md)
- [Testing](./testing.md)
- Integrations
  - [HTTP Client](./13-client.md)
  - [HTTP/2 Client and Server](./14-h2.md)
  - [gRPC Services and Clients](11-grpc.md)
  - [MQTT Client and Server](15-mqtt.md)
- [Securing Connections with TLS](./10-tls.md)
- [Byte Buffers](./9-bytes.md)
- [Timers and Time Utilities](./12-time.md)
- [Cookbook](./cookbook.md)

## API reference

- [API documentation for the `main` branch](https://docs.ntex.rs/api/)
- [API documentation for released versions on docs.rs](https://docs.rs/ntex/)

## LLM documentation

- [llms.txt](https://docs.ntex.rs/llms.txt): rules and templates for generated code
- [llms-full.txt](https://docs.ntex.rs/llms-full.txt): this guide as a single text file

## Migration

- [Migrating from ntex 3 to ntex 4](../ntex/migration_3to4.md)
