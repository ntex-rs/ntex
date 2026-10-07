# Migrating from ntex 3 to ntex 4

ntex 4 requires Rust 1.97 and upgrades the service stack to `ntex-service` 5.
Most migration work involves the new service state and lifecycle APIs.

## Server

Server service factories now create a `Service` directly. They no longer
create a `ServiceFactory` whose initialization configuration is `SharedCfg`.
The factory is called once per worker and receives a reference to that
worker's application state.

For a server without custom application state, continue to use
`ntex::server::build()`. The factory receives `&()`:

```rust,no_run
use std::io;
use ntex::http::{HttpService, Response};
use ntex::SharedCfg;

#[ntex::main]
async fn main() -> io::Result<()> {
    ntex::server::build()
        .bind(
            "http",
            "127.0.0.1:8080",
            SharedCfg::default(),
            async |_| {
                HttpService::new(async |_| {
                    Ok::<_, io::Error>(Response::Ok().body("Hello"))
                })
            },
        )?
        .run()
        .await
}
```

Use `ntex::server::build_with_config()` when each worker needs application
state. Its argument implements `ServerAppConfig` and creates the state for each
worker. An asynchronous closure can be used directly:

```rust,no_run
use std::io;
use ntex::SharedCfg;

#[derive(Clone)]
struct WorkerState;

#[ntex::main]
async fn main() -> io::Result<()> {
    ntex::server::build_with_config(
        async || Ok::<_, io::Error>(WorkerState),
    )
    .bind(
        "service",
        "127.0.0.1:8080",
        SharedCfg::default(),
        async |state: &WorkerState| {
            let state = state.clone();
            ntex::service::fn_service(move |_| {
                let _state = state.clone();
                async { Ok::<_, io::Error>(()) }
            })
        },
    )?
    .run()
    .await
}
```

`ntex-service` 5 also removes `Service::poll()`. The `Service` trait now uses
the asynchronous `ready()` and `shutdown()` lifecycle methods, while service
chains provide `readiness()` and `shutdown()` callbacks. `Pipeline` and
middleware APIs have been updated to bind and propagate service state.
`Pipeline::call_nowait()` and `PipelineBinding::call_nowait()` have been
removed; use `call()`, which skips the readiness check when the last pipeline
readiness check has succeeded and no other call has consumed it.

### Server builder

Several `ServerBuilder` and web `HttpServer` methods have been renamed or
removed:

| ntex 3 | ntex 4 |
|--------|--------|
| `maxconn()` | `max_connections()` |
| `shutdown_timeout()` | `graceful_shutdown_timeout()` |
| `HttpServer::maxconnrate()` | `HttpServer::max_tls_handshakes()` |
| `ServerBuilder::config()`, `HttpServer::config()` | pass `SharedCfg` to `bind()` or `listen()` |
| `on_worker_start()`, `on_accept()` | `build_with_config()` or `HttpServer::with_config()` |

`WorkerPool::shutdown_timeout()` is also renamed to
`graceful_shutdown_timeout()`.

### Runtime features

The deprecated `neon` feature has been removed from `ntex`, `ntex-rt` and
`ntex-net`, the native runtime is used without it. Remove it from `ntex` and
direct `ntex-rt` or `ntex-net` dependencies. The `neon-iocp` feature has been
removed as well, Windows always uses IOCP. Select the `tokio`, `compio`,
`neon-polling` or `neon-uring` feature when a specific runtime backend is
required.

## HTTP services

`HttpService::openssl()` and `HttpService::rustls()` have been replaced by the
`http::openssl()` and `http::rustls()` service wrappers.

OpenSSL:

```rust,ignore
let service = ntex::http::openssl(
    acceptor,
    ntex::http::HttpService::new(handler),
);
```

rustls now accepts the ALPN protocol names separately:

```rust,ignore
let service = ntex::http::rustls(
    config,
    &["h2", "http/1.1"],
    ntex::http::HttpService::new(handler),
);
```

### HTTP configuration

* `HttpServiceConfig::set_enable_headers_vec()` is replaced by
  `set_headers_vec(bool)`.
* `h1::Codec::upgrade()` has been removed; use `Request::upgrade()` to detect
  upgrade and `CONNECT` requests.
* New settings: `set_half_close()`, `set_host_validation()`,
  `set_max_start_line_size()`, and `set_write_timeout()`.

## URLs

ntex 4 uses `urly::Url`, re-exported as `ntex::url`, instead of `http::Uri`:

* `ntex::http::Uri` and the `ntex::http::uri` module have been removed.
* `RequestHead::uri`, `Request::uri()`, `HttpRequest::uri()`, and
  `WebRequest::uri()` are `Url` values. `Url::path()` and `Url::query()`
  return `ntex::url::Path` and `ntex::url::Query`; use `as_str()` to get
  string slices.
  `HttpRequest::path()` and `HttpRequest::query_string()` still return `&str`.
* `HttpRequest::url_for()` returns `ntex::url::Url` instead of `url::Url`.
* The `url` feature has been removed; URL support is always available.
* The HTTP and WebSocket clients accept any type that converts to `Url`,
  including `&str`, `String`, and `http::Uri`. Client connectors use
  `Connect<Url>` instead of `Connect<Uri>`.
* Server request targets are normalized: dot segments are removed, a fragment
  is dropped, and an origin-form path starting with `//` is a path, not an
  authority.

## HTTP client

The HTTP client builder, connector, and connection pool have been redesigned:

* `ClientBuilder` now owns the connector configuration; the separate client
  connector builder has been removed.
* Custom connector factories are replaced by connector `Service` values.
* `ClientBuilder::build()` is synchronous and takes a `SharedCfg`.
* Client settings are stored in `ClientConfig` inside `SharedCfg`.

The default connector is configured automatically:

```rust
use ntex::SharedCfg;
use ntex::client::{Client, ClientConfig};

fn create_client() -> Client {
    let cfg = SharedCfg::new("client").add(
        ClientConfig::new()
            .set_h1_connection_limit(8)
            .set_response_payload_limit(256 * 1024),
    );

    Client::builder().build(cfg)
}
```

Use `ClientBuilder::connector()` or `ClientBuilder::secure_connector()` to
install a custom connector service.

Request defaults have moved from `ClientBuilder` to `ClientConfig`:

| ntex 3 `ClientBuilder` | ntex 4 `ClientConfig` |
|------------------------|-----------------------|
| `header()` | `set_header()` |
| `basic_auth()`, `bearer_auth()` | `set_basic_auth()`, `set_bearer_auth()` |
| `response_timeout()`, `disable_timeout()` | `set_response_timeout()`, `disable_timeout()` |
| `response_payload_limit()` | `set_response_payload_limit()` |
| `response_payload_timeout()` | `set_response_payload_timeout()` |

The `ClientConfig` getters `timeout()`, `payload_limit()`, and
`payload_timeout()` are now `response_timeout()`, `response_payload_limit()`,
and `response_payload_timeout()`.

`ClientBuilder::disable_redirects()`, `max_redirects()`, and
`no_default_headers()` have been removed.

The error variants `ClientError::TunnelNotSupported`, `ConnectError::Timeout`,
`ConnectError::SslError`, `ConnectError::SslHandshakeError`, and
`EncodeError::Fmt` have been removed. `InvalidUrl::Http` is replaced by
`InvalidUrl::Parse`.

## WebSocket client

WebSocket client settings have moved to `WsClientConfig`. Construct
`WsClient` directly with the URI and configuration; a separate builder is no
longer required:

```rust,ignore
use ntex::ws::{WsClient, WsClientConfig};

let client = WsClient::new(
    "ws://127.0.0.1:8080/ws",
    WsClientConfig::new().set_max_frame_size(128 * 1024),
);

let connection = client.connect().await?;
```

URL validation errors are now reported by `connect()` rather than by
`WsClient::new()`, as `WsConfigError::Parse` instead of `WsConfigError::Http`.

Custom connectors and TLS are still selected with `connector()`, `openssl()`,
or `rustls()` on `WsClient`.

`WsSink::on_disconnect()` now returns `ntex::io::Waiter<'static>`; the
`OnDisconnect` future type has been removed.

## Web applications

Web application state and error handling have been redesigned.

### Application state in handlers

The application state type implements `web::State`. For common state that uses
the default web error type, `web::AppState<T>` provides a ready-made wrapper.
State is created once per worker with `web::server_with_config()` and is
passed to state-aware handlers by reference.

The `web::types::State<St>` extractor has been removed. Replace handlers that
use it with `Route::to_with_state()`. A state-aware handler receives:

1. A shared reference to the application state.
2. The request-local state.
3. Any request extractors.

```rust,no_run
use std::io;
use ntex::{web, SharedCfg};

#[derive(Clone)]
struct TestAppState {
    value: &'static str,
}

impl web::State for TestAppState {
    type Error = web::DefaultError;
}

async fn index(
    state: &TestAppState,
    _request_state: (),
) -> web::HttpResponse {
    web::HttpResponse::Ok().body(state.value)
}

#[ntex::main]
async fn main() -> io::Result<()> {
    web::server_with_config(
        async || {
            Ok::<_, io::Error>(TestAppState {
                value: "Hello",
            })
        },
        async |_| {
            web::App::<TestAppState>::new()
                .route("/", web::get().to_with_state(index))
        },
    )
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

The state-aware variants are available as `web::to_with_state()`,
`Route::to_with_state()`, `Resource::to_with_state()`, and
`ResourceServices::to_with_state()`. Continue to use `to()` when a handler only
needs request extractors.

The old `App::state()` model is replaced by service state for the primary
application state. Additional configuration values can be stored with
`WebAppConfig::set_state()` and retrieved with `HttpRequest::app_state()`.
Request-local state is available through `WebRequest::st()` and
`WebRequest::st_mut()`. Filters and middleware can change its type with
`WebRequest::map_state()`.

Custom implementations of the `Handler` trait must change `call()` from a
method returning `impl Future` to an `async fn`. Ordinary `async fn` handlers
do not need this change.

### Error handling

The `ErrorRenderer` API has been removed. The state's associated `Error` type
defines the application's error type, and errors are rendered through
`WebResponseError<St, Err>`. Error rendering receives the application state
instead of an `HttpRequest`. Service initialization errors now use
`ntex::error::Failure` and `IntoFailure`.

## Custom codecs and I/O

This section applies to code that implements codecs, filters, or dispatchers
directly on top of `ntex::io` and `ntex::codec`.

### Codecs

`ntex-codec` 2 writes encoded data into `BytePages`. The deprecated
`encode(BytesMut)` method has been removed and `encodev()` has been renamed to
`encode()`, which is now required:

```rust,ignore
impl Encoder for MyCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&self, item: Bytes, dst: &mut BytePages) -> Result<(), io::Error> {
        dst.append(item);
        Ok(())
    }
}
```

`Decoder::decode_eof()` is called when the peer closes the stream. Its default
implementation calls `decode()`; override it to decode a final frame that has
no terminator. If undecodable bytes remain after a clean EOF, `Io::recv()` and
the dispatcher report an `io::ErrorKind::UnexpectedEof` error.

### `IoConfig`

* `set_disconnect_timeout()` / `disconnect_timeout()` are renamed to
  `set_shutdown_timeout()` / `shutdown_timeout()`. A zero timeout panics.
* Read and write buffers are `ntex-bytes` pages and share one page cache,
  configured per thread with `ntex_bytes::set_page_cache_size()`. `BufConfig`
  and `IoConfig::read_buf()` / `write_buf()` have been removed.
* Each connection adapts its read page size to its reads, between the sizes set
  with `set_read_size(min, max)` (4 KiB to 64 KiB by default).
* Backpressure is configured with a single high watermark and is released at
  half of it:

| ntex 3 | ntex 4 |
|--------|--------|
| `set_read_buf(high, low, cache_size)` | `set_read_backpressure(high)` and `set_read_size(min, max)` |
| `set_write_buf(high, low, cache_size)` | `set_write_backpressure(high)` |
| `set_write_page_size()`, `write_page_size()` | `set_write_size()`, `write_size()` |

* `set_write_timeout()` closes connections whose peer stops reading.

### `Io` and `IoRef`

| ntex 3 | ntex 4 |
|--------|--------|
| `IoRef::force_close()` | `IoRef::terminate()` |
| `IoRef::wants_shutdown()` | `IoRef::close()` |
| `IoRef::with_read_buf()` | `IoRef::with_read_dst()` |
| `IoRef::with_read_src_buf()` | `IoRef::with_read_src()` |
| `IoRef::with_write_buf()` | `IoRef::with_write_src()` |
| `IoRef::with_write_dst_buf()` | `IoRef::with_write_dst()` |
| `IoRef::on_disconnect()` returning `OnDisconnect` | returns `Waiter<'static>` |
| `Io::read_ready()`, `Io::poll_read_ready()` | `Io::read_more()`, `Io::poll_read_more()` |
| `Io::poll_dispatch()` | `Io::register_dispatch()` |
| `Io::pause()` | removed; reads resume via `poll_read_more()` |
| `Io::set_config()` | pass the configuration to `Io::new()` |
| `Io::take()` | `unsafe Io::take()` |
| `IoRef::resize_read_buf()`, `IoContext::resize_read_buf()` | `BytesMut::reserve_more()` |
| `IoStatusUpdate::KeepAlive` | `IoStatusUpdate::Timeout` |

`IoRef::is_closed()` now reports whether closing has finished. Use the new
`IoRef::is_active()` to check whether the connection is still usable.

### Dispatcher

`Reason::KeepAliveTimeout` is renamed to `Reason::KeepAlive`, and the new
`Reason::WriteTimeout` is reported when `IoConfig::set_write_timeout()`
expires.

## Other API changes

* `ntex::rt`: `System::stop_on_panic()` has been removed and
  `Builder::stop_on_panic()` no longer has an effect. Use
  `Builder::panic_handling()` or `#[ntex::main(panic_handling = true)]`.
  `System::set_latency_callback()` requires a `Send + Sync` callback.
* `ntex::time`: `query_system_time()` has been removed; use `system_time()`.
* `ntex_util::channel::bstream::Receiver::max_buffer_size()` is deprecated in
  favor of `set_watermarks()`.
* `ntex::http::HeaderMap` no longer implements `FromIterator`; build maps with
  `insert()` or `append()`.
* `HeaderValue::to_str()` accepts any valid UTF-8 value, not only visible
  ASCII.

### Router

`ntex::router` (`ntex-router` 2) renames several APIs:

| ntex 3 | ntex 4 |
|--------|--------|
| `Router::recognize_mut_checked()` | `Router::recognize_checked_mut()` |
| `ResourceDef::resource_path()`, `resource_path_named()` | `ResourceDef::build_path()`, `build_path_named()` |
| `ResourceDef::name_mut()` | `ResourceDef::set_name()` |
| `RouterBuilder::rdef()` | `RouterBuilder::resource()` |
| `Path::unprocessed()` | `Path::path()` |
| `Path::query()` | `Path::get()` |
| `Router::build()`, `RouterBuilder::finish()` | removed |

`RouterBuilder` registration methods return `&mut RouterEntry` instead of a
tuple; use `set_id()`, `set_name()`, `set_check_value()`, and
`resource_mut()`. `Path::skip()` takes a `u32`. Path segments that do not
decode to valid UTF-8 are kept percent-encoded.

## Connection and protocol configuration

`SharedCfg` remains the container for connection and protocol configuration,
but it is separate from worker application state:

* Pass `SharedCfg` to server `bind()` or `listen()` methods.
* Pass `SharedCfg` to `ClientBuilder::build()`.
* Use `build_with_config()` or `web::server_with_config()` for per-worker
  application state.

See
[Connection and Protocol Configuration](docs/5-server.md#connection-and-protocol-configuration)
for a complete example.
