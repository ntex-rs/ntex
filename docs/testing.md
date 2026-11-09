# Testing

Start with the smallest test that exercises the behavior you care about. For
most web code, that means sending an in-memory request through an application,
not starting a server. You still exercise routing, extractors, middleware, and
response rendering, but there is no listener to configure or port to reserve.

Use `#[ntex::test]` for asynchronous tests. It supplies the ntex runtime, so
application services, I/O tasks, and timers run in the same environment as
other ntex code. Each example below is a complete test or a complete program
containing tests. The examples use ntex with the `tokio` feature; the JSON
examples also need `serde` with `derive` and `serde_json`, and the WebSocket
example needs ntex's `ws` feature.

## Start with an in-process application

`web::test::init_service` builds an application pipeline without opening a
socket. Build a request with `TestRequest`, turn it into an HTTP request with
`to_request()`, and pass it to `call_service`:

```rust
use ntex::http::StatusCode;
use ntex::web::{self, App, test};

async fn health() -> &'static str {
    "ready"
}

#[ntex::test]
async fn health_returns_a_body() {
    let app = test::init_service(
        App::new().route("/health", web::get().to(health)),
    )
    .await;

    let req = test::TestRequest::get().uri("/health").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(test::read_body(res).await, "ready");
}
```

Check the status before consuming the response with `read_body`. That helper
collects the entire body into `Bytes`; it is convenient for small text and
binary responses, but it does not prove that a streaming response arrives
incrementally. Reading the body also catches failures that occur while
producing it rather than while selecting the response.

This test runs the handler through the router. Calling `health().await`
directly would test only its return value, not whether `/health` is registered
for `GET`. A direct handler call can still help when the handler has substantial
logic, but use an application test for the HTTP contract. See
[Routing and Handlers](./8.1-web-app.md) for registration patterns.

Choose the request conversion for the layer being tested. The web builder's
`to_http_request()` makes an `HttpRequest` for a direct handler call;
`to_srv_request()` makes a `WebRequest` for lower-level middleware tests.
Neither conversion runs application routing. For an app pipeline, use
`to_request()` as above. If you work directly with the lower-level HTTP API,
`http::test::TestRequest` has a different terminal method, `build()`, which
produces an `http::Request`.

`init_service` succeeding does not prove every route can be constructed.
Registered services are created on their first request; a failing service
factory produces a `500 Internal Server Error` response at that point. Send a
request to each route you need to verify, and assert the resulting status.

## Share routes with the executable

A test can pass while production is broken if it registers a different set of
routes. Put route registration in a configuration function, then call that
function from both `main` and the tests. Keep the tests in a `#[cfg(test)]`
module at the bottom of the file so they can use the same handlers and types.

The following program exposes a JSON preview endpoint. It returns the supplied
note without storing it. Its tests cover successful serialization and a body
that cannot be parsed:

```rust
use ntex::{SharedCfg, web};
use ntex::web::{App, HttpResponse, types::Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct Note {
    title: String,
}

async fn preview(note: Json<Note>) -> HttpResponse {
    HttpResponse::Ok().json(&note.into_inner())
}

fn config(cfg: &mut web::ServiceConfig) {
    cfg.route("/preview", web::post().to(preview));
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| App::new().configure(config))
        .bind("127.0.0.1:8080", SharedCfg::default())?
        .run()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::http::StatusCode;
    use ntex::web::test;

    #[ntex::test]
    async fn preview_round_trips_json() {
        let app = test::init_service(App::new().configure(config)).await;
        let note = Note { title: "First draft".into() };
        let req = test::TestRequest::post()
            .uri("/preview")
            .json(&note)
            .to_request();

        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = test::read_body(res).await;
        let returned: Note = serde_json::from_slice(&body).unwrap();
        assert_eq!(returned, note);

        let req = test::TestRequest::post()
            .uri("/preview")
            .json(&note)
            .to_request();
        let returned: Note = test::read_response_json(&app, req).await;
        assert_eq!(returned, note);
    }

    #[ntex::test]
    async fn malformed_json_is_rejected() {
        let app = test::init_service(App::new().configure(config)).await;
        let req = test::TestRequest::post()
            .uri("/preview")
            .header("content-type", "application/json")
            .payload("{")
            .to_request();

        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(!test::read_body(res).await.is_empty());
    }
}
```

`TestRequest::json(&value)` serializes the value and sets the JSON content
type. To test malformed input, use `payload` and set the content type yourself;
serializing a Rust value cannot produce invalid JSON. `form(&value)` is the
corresponding helper for URL-encoded form bodies. Headers, methods, and query
strings belong in the test request too, not just in direct extractor tests.

There are two JSON-reading patterns above. After `call_service`, deserialize
`read_body(res)` yourself when you need to inspect the status or headers.
`read_response_json(&app, req)` instead sends the request and deserializes its
body in one step. It takes an application and a request, not an already-read
response, and does not expose the status. The second request demonstrates that
shortcut; it is not needed to establish the first request's HTTP contract.

The malformed-body test checks the status and that the response contains an
explanation, without depending on the JSON parser's exact wording. When your
application promises a particular error schema or message, assert that schema
or message instead. Add separate requests for other boundaries your endpoint
has, such as a missing required field, a payload limit, an unknown path, or a
method that is not registered. See
[Extractors and Responders](./8.4-web-app.md) for the corresponding request
rules.

## Test with application state

Use `init_service_st(state, app)` when the app has a state type other than
`()`. Supply the same kind of state that the server supplies, but build fresh
values for each test. A retained clone of a shared handle lets you check both
the HTTP response and the state change.

This counter uses `AppState<Rc<Cell<usize>>>`. Cloning it shares the counter;
it does not copy the numeric value into an independent counter:

```rust
use std::{cell::Cell, rc::Rc};

use ntex::http::StatusCode;
use ntex::web::{self, App, AppState, test};

type Hits = AppState<Rc<Cell<usize>>>;

async fn hit(hits: &Hits, (): ()) -> String {
    hits.set(hits.get() + 1);
    hits.get().to_string()
}

fn config(cfg: &mut web::ServiceConfig<Hits>) {
    cfg.route("/hit", web::post().to_with_state(hit));
}

#[ntex::test]
async fn requests_share_the_supplied_counter() {
    let hits = Hits::default();
    let app = test::init_service_st(
        hits.clone(),
        App::<Hits>::new().configure(config),
    )
    .await;

    for expected in ["1", "2"] {
        let req = test::TestRequest::post().uri("/hit").to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(test::read_body(res).await, expected);
    }
    assert_eq!(hits.get(), 2);
}
```

`to_with_state` passes `&Hits` separately from extracted request arguments.
Here there are no request arguments, so the second argument is `()`. This
keeps the test setup close to the handler's actual dependencies. For a database
or another external dependency, use a test implementation behind your own
state interface, or isolate a dedicated integration fixture; do not make a
routine route test depend on an external service being available.

`Rc<Cell<_>>` is suitable for this worker-local, in-process example. A real
test server runs in another thread, so you cannot capture this handle in its
`Send` factory. Create worker-local state inside the server factory, or use
appropriate `Arc`-based state when the test and worker must share it. The
[Application State](./8.2-web-app.md) chapter explains the distinction between
worker state and request state in more detail.

## Exercise middleware and error responses

Test middleware around an application, not only around a hand-built handler
result. That checks its position relative to routing and error rendering.
Include a request that reaches the handler and one that takes the rejection
path.

This example requires a particular test API key and marks successful responses
with an application header. It is a small middleware example, not a complete
authentication scheme:

```rust
use ntex::http::{StatusCode, header::{HeaderName, HeaderValue}};
use ntex::web::{self, App, WebError, error, middleware::from_fn, test};

async fn report() -> &'static str {
    "report ready"
}

#[ntex::test]
async fn middleware_allows_a_key_and_renders_rejections() {
    let app = test::init_service(
        App::new()
            .middleware(from_fn(async |req, next| {
                let allowed = req.headers().get("x-api-key")
                    .is_some_and(|value| value == "test-key");
                if !allowed {
                    return Err(WebError::from_err(
                        error::ErrorForbidden("API key required"),
                    ));
                }
                let mut res = next.call(req).await?;
                res.headers_mut().insert(
                    HeaderName::from_static("x-application"),
                    HeaderValue::from_static("reports"),
                );
                Ok(res)
            }))
            .route("/report", web::get().to(report)),
    )
    .await;

    let req = test::TestRequest::get().uri("/report").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(test::read_body(res).await, "API key required");

    let req = test::TestRequest::get()
        .uri("/report")
        .header("x-api-key", "test-key")
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("x-application").unwrap(), "reports");
    assert_eq!(test::read_body(res).await, "report ready");
}
```

A rendered HTTP failure is normally a response you inspect, not an `Err` you
expect from `call_service`. That helper unwraps pipeline errors. The test
above checks the `403` and body the client would see after ntex renders the
middleware's `WebError`. For custom handler errors, do the same through a route;
a unit test of the error's `Display` implementation cannot verify the selected
HTTP status or the configured error domain.

Keep error assertions appropriate to the app's public contract. Internal
failures should not accidentally become successful responses, and private
diagnostic details should not become part of a stable response schema. For
response-path ordering and custom error domains, see
[Filters and Middleware](./8.3-web-app.md) and
[Web Application Errors](./8.5-web-app.md).

## Test a plain service with a pipeline

Services do not need HTTP requests or sockets. Wrap a service in
`Pipeline::new(state, service)` and call it with its ordinary request type.
The pipeline supplies the service context and waits for readiness before
calling the service. This is also the right level to test a reusable component
before putting it behind a web route or protocol dispatcher.

```rust
use std::io;

use ntex::service::{Pipeline, fn_service};

#[ntex::test]
async fn greeting_service_validates_names() {
    let service = fn_service(async |name: String| {
        let name = name.trim();
        if name.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty name"));
        }
        Ok::<_, io::Error>(format!("Hello, {name}!"))
    });
    let pipeline = Pipeline::new((), service);

    assert_eq!(pipeline.call(" Ada ".into()).await.unwrap(), "Hello, Ada!");
    let err = pipeline.call("  ".into()).await.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    pipeline.shutdown().await;
}
```

The first argument to `Pipeline::new` is service state, not the request. Use
`()` when none is needed. A function service needs an explicit result error
type, as in `Ok::<_, io::Error>`, so type inference has enough information.

Unlike the application test, the failure here is an actual service error, so
`unwrap_err()` is appropriate. HTTP error rendering has not happened. See the
[Service Model](./1-service.md) and [Service Pipelines](./2-pipeline.md) chapters
for named services, composition, readiness, and shutdown. Test readiness or
shutdown explicitly when your own service has behavior there; successful
request assertions alone do not cover those hooks.

## Test protocols with in-memory I/O

`IoTest::create()` returns a connected pair of in-memory transports. Wrap the
server end in `Io`, and use the client end to supply bytes and read replies.
This exercises your protocol's real receive/send code without involving TCP.

```rust
use ntex::codec::BytesCodec;
use ntex::io::{Io, testing::IoTest};
use ntex::util::Bytes;

async fn echo_once(io: Io) {
    let bytes = io.recv(&BytesCodec).await.unwrap().unwrap();
    io.send(bytes, &BytesCodec).await.unwrap();
}

#[ntex::test]
async fn byte_protocol_echoes_in_memory() {
    let (client, server) = IoTest::create();
    client.remote_buffer_cap(1024);
    let task = ntex::rt::spawn(echo_once(Io::from(server)));

    client.write(b"ping");
    assert_eq!(client.read().await.unwrap(), Bytes::from_static(b"ping"));
    task.await.unwrap();
}
```

Set `remote_buffer_cap` on the client to allow the server transport to write
back. The default capacity is zero, so omitting it can leave an otherwise
correct reply waiting for write capacity. `write` makes input available to
the peer and wakes its reader; `read().await` waits for output. No sleep is
needed to give the server task a chance to run. Retaining and awaiting the task
also lets a panic in the protocol handler fail the test.

`BytesCodec` passes through available byte chunks. It does not impose message
boundaries, and neither TCP nor `IoTest::read` promises one read per application
message. For a framed protocol, use the same framing codec as production,
feed an incomplete frame and then its remainder, and test several frames
arriving together. Keep small decoder-only tests for invalid and truncated
frames as well; those do not need an I/O task at all.

`IoTest` can also inject transport failures with `read_error` and `write_error`,
or constrain write capacity to exercise backpressure. Add those cases when
your protocol handles them differently. The
[I/O Abstraction Layer](./7-io.md#testing) chapter covers the transport and
filter APIs used by these tests.

## Use a real server for connection behavior

Move to a real listener when the behavior depends on the connection: HTTP
client integration, streaming delivery, WebSocket upgrades, TLS negotiation,
or connection shutdown. Keep ordinary route validation in-process so those
tests stay small and independent of the networking stack.

Choose the helper at the layer you are testing:

- `web::test::server` takes a factory that returns an application.
- `http::test::server` takes a factory that returns an I/O service, such as an
  `HttpService`, for lower-level HTTP tests.
- `server::test_server` takes an I/O service factory for a custom TCP protocol.

These factories take `async |_| ...`; the argument is `&()`, even when it is
unused. The helpers bind an available loopback port. Use the returned server's
address or URL instead of reserving a fixed port in the test.

### Send a request with the HTTP client

Use an explicit client when the client configuration or client/server
interaction is what you want to test:

```rust
use ntex::client::Client;
use ntex::http::StatusCode;
use ntex::web::{self, App, test};

async fn health() -> &'static str {
    "ready"
}

#[ntex::test]
async fn client_receives_a_complete_response() {
    let server = test::server(async |_| {
        App::new().route("/health", web::get().to(health))
    });
    let client = Client::new();
    let response = client.get(server.url("/health")).send().await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().await.unwrap(), "ready");
}
```

The server controller keeps the listener alive and stops the server when
dropped. The helper also offers `server.get(path)` and other request builders
using its own client. Use those when you do not need a separately configured
client. See [Making HTTP Requests](./13-client.md) for client timeouts, JSON
bodies, TLS connectors, and streaming responses.

Collecting `response.body()` is enough to check the complete payload, but it
cannot prove when chunks arrived. A streaming test should consume the response
stream and coordinate the producer with explicit signals, asserting a chunk
before releasing the next one. For TLS, use a local test certificate and a
client that trusts that certificate or test CA; do not depend on a public HTTPS
site. See [Securing Connections with TLS](./10-tls.md) for connector setup.

### Exchange WebSocket frames

A WebSocket test needs an actual upgrade and an upgraded connection. The test
server's `ws_at` helper creates a WebSocket client for the selected route. This
example checks both the echoed text frame and a ping/pong exchange:

```rust
use ntex::util::Bytes;
use ntex::web::{self, App, HttpRequest, test, ws};

async fn echo(frame: ws::Frame) -> Result<Option<ws::Message>, std::io::Error> {
    Ok(match frame {
        ws::Frame::Text(text) => Some(ws::Message::Text(
            String::from_utf8_lossy(&text).as_ref().into(),
        )),
        ws::Frame::Ping(bytes) => Some(ws::Message::Pong(bytes)),
        ws::Frame::Close(reason) => Some(ws::Message::Close(reason)),
        _ => None,
    })
}

async fn socket(req: HttpRequest) {
    let _ = ws::start(&req, None, echo).await;
}

#[ntex::test]
async fn websocket_echoes_text_and_answers_ping() {
    let server = test::server(async |_| {
        App::new().route("/ws", web::get().to(socket))
    });
    let (io, codec, _) = server.ws_at("/ws").await.unwrap().into_inner();

    io.send(ws::Message::Text("hello".into()), &codec).await.unwrap();
    assert_eq!(
        io.recv(&codec).await.unwrap(),
        Some(ws::Frame::Text(Bytes::from_static(b"hello"))),
    );

    io.send(ws::Message::Ping(Bytes::from_static(b"probe")), &codec)
        .await
        .unwrap();
    assert_eq!(
        io.recv(&codec).await.unwrap(),
        Some(ws::Frame::Pong(Bytes::from_static(b"probe"))),
    );
}
```

A successful `ws_at` call verifies that the handshake succeeded; receiving the
expected frames checks the upgraded protocol. Add close, binary, fragmentation,
or rejection cases only when they are part of your application's behavior.
See [WebSocket Support](./8.6-web-app.md) for frame handling and direct
`WsClient` configuration.

## Keep asynchronous tests deterministic

Wait for the event you need: a response, a channel message, a task's completion,
or readable bytes. Sleeping for a guessed duration makes a test slow on a fast
machine and unreliable on a busy one. Use a generous timeout as a failure bound
when an operation might otherwise wait forever, not as evidence that another
task has reached a particular point.

The ntex time utilities expose `Sleep::elapse()` and `TimerHandle::elapse()` to
complete an individual timer immediately. If your own component accepts such
a timer, retain access to it and trigger it when the test needs expiry. There
is no public pause-and-advance test clock in `ntex-util`: these methods do not
advance every timer, change `now()`, or expire an unrelated network timeout.
For other timing-dependent logic, separate the decision from the wait or pass
in a controllable dependency. See [Timers and Time Utilities](./12-time.md)
for the timer APIs.

Give every test fresh application state, keep real listeners on dynamically
assigned ports, and assert the exact status your client relies on. A green
test should mean the intended route and response worked, not merely that the
application builder returned successfully.
