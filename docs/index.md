# ntex framework guide

Learn how ntex services fit together, run servers, manage state and I/O, and
build web applications.

- [Service Model](./1-service.md)
- [Service Pipelines](./2-pipeline.md)
- [Service State](./3-state.md)
- [Runtime](./4-runtime.md)
- [Server](./5-server.md)
- [Worker and Request State](./6-server-app.md)
- [I/O Abstraction Layer](./7-io.md)
- Web Application Framework
  - [Applications and Routing](./8.1-web-app.md)
  - [Application State](./8.2-web-app.md)
  - [Filters and Middleware](./8.3-web-app.md)
  - [Extractors and Responders](./8.4-web-app.md)
  - [Error Handling](./8.5-web-app.md)
  - [WebSocket Support](./8.6-web-app.md)
- Protocols
  - [Building gRPC Services and Clients](11-grpc.md)
- [Securing Connections with TLS](./10-tls.md)
- [Byte Buffers](./9-bytes.md)
- [Timers and Time Utilities](./12-time.md)

## API reference

- [API documentation for the `main` branch](https://docs.ntex.rs/api/)
- [API documentation for released versions on docs.rs](https://docs.rs/ntex/)

## LLM documentation

- [llms.txt](https://docs.ntex.rs/llms.txt): rules and templates for generated code
- [llms-full.txt](https://docs.ntex.rs/llms-full.txt): this guide as a single text file

## Migration

- [Migrating from ntex 3 to ntex 4](../ntex/migration_3to4.md)
