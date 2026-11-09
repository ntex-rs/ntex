# Making HTTP Requests

Very few services live alone. Sooner or later yours will need to talk to a
payment provider, an internal API, an object store, or someone's webhook.
`ntex::client` is the tool for that job.

If you have built a server with ntex, the client will feel familiar. It is
made of the same pieces: [services](1-service.md), the
[I/O layer](7-io.md), and shared configuration. It speaks HTTP/1.1 and HTTP/2,
keeps connections open and reuses them, and supports TLS through OpenSSL or
rustls. Optional features add response decompression and cookies, and
middleware lets you hook your own logic into every request.

Making a request comes down to three steps: build it, send it, and read what
comes back.

```rust,no_run
use ntex::client::Client;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();

    let response = client
        .get("http://127.0.0.1:8080/")
        .header("user-agent", "ntex-guide")
        .send()
        .await?;

    println!("status: {}", response.status());
    let body = response.body().await?;
    println!("body: {}", String::from_utf8_lossy(&body));
    Ok(())
}
```

That is all you need for simple cases. The rest of this chapter looks at each
step more closely, starting with what happens inside the client.

## How the client works

A `Client` is not meant to be created per request. It owns connection pools
and configuration, and its value comes from reusing them. An application
usually creates one client for each kind of outgoing traffic, such as one for
the payment provider and another for an internal API, each with its own
timeouts and limits. It then keeps those clients for its whole lifetime.

A `Client` is a lightweight handle. Cloning it is cheap, and all clones share
the same connection pools, so you can hand copies to every part of your code
that needs one. The handle is not `Send`, though: it belongs to the thread
that created it. In a server, the usual pattern is to create the clients once
per worker, for example in the application factory, and keep them in
application state.

When you send a request, it passes through a short chain of services:

```text
ClientRequest ─► middleware ─► Sender ─► Connector ─┬─► plain pool ─► HTTP/1.1 connections
                                                    └─► TLS pool   ─► HTTP/1.1 or HTTP/2 connections
```

It starts as a `ClientRequest`, the builder you get from `client.get(...)`
and friends, which collects the method, URL, headers, and body. Next comes
any [middleware](#middleware) you installed; it can inspect or change the
request on its way out and the response on its way back. Then the `Sender`
takes over. It asks the `Connector` for a connection, writes the request, and
waits for the response head.

The `Connector` chooses a pool based on the URL scheme: `http` goes to
the plain pool, `https` to the TLS pool. Each pool keeps
connections grouped by host. An idle HTTP/1.1 connection is reused for the
next request to the same host. An HTTP/2 connection can carry many requests
at once, so concurrent requests to the same host share it.

You don't pick the HTTP version yourself. For `https` URLs, client and server
settle it during the TLS handshake using ALPN: the client offers `h2` and
`http/1.1`, and the server chooses. Plain `http` URLs always use HTTP/1.1.

One detail is worth remembering. An HTTP/1.1 connection goes back to the
pool only after its response body has been read to the end. If you drop a
response without reading the body, the client can't tell what is left on the
wire, so it closes the connection instead. If you want connections reused,
read the body, even when you don't care what's in it.

You don't have to shut the client down explicitly. Once the last clone and
every request made with it are dropped, the pools close idle connections in
the background, and HTTP/2 connections close gracefully after their in-flight
requests finish.

The client is for requests and responses. WebSockets have their own client,
`ntex::ws::WsClient`, covered in
[WebSocket Support](8.6-web-app.md#clients-with-ntexws).

## Configuration

`Client::new()` uses sensible defaults, and for a quick script they are
fine. For a real service you will want to tune at least the timeouts.

Configuration works the same way as on an ntex server. You put
configuration objects into a [`SharedCfg`] and build the client from it. A
single `SharedCfg` can hold several configuration types, and each part of the
client picks out the one it cares about:

| Configuration | Read by | What it controls |
|---|---|---|
| [`ClientConfig`] | The client | Timeouts, payload limits, pool sizes, default headers |
| [`IoConfig`] | Connector and connections | Connect timeout, buffer sizes, backpressure, shutdown timeout |
| [`TlsConfig`] | TLS connectors | TLS handshake timeout |
| [`HttpServiceConfig`] | HTTP/1.1 response parser | Maximum number of headers and size of the response head |
| [`Http2ServiceConfig`] | HTTP/2 connections | Window sizes, frame size, ping and handshake timeouts |

Anything you leave out keeps its defaults. The name you give the `SharedCfg`
shows up in logs, so choose one that tells you which client is talking.

### Client settings

Most of the settings you will reach for live in `ClientConfig`:

```rust
use ntex::client::{Client, ClientConfig};
use ntex::http::error::HttpError;
use ntex::{SharedCfg, time::Seconds};

fn api_client(token: &str) -> Result<Client, HttpError> {
    let cfg = SharedCfg::new("API-CLIENT").add(
        ClientConfig::new()
            .set_response_timeout(Seconds(10))
            .set_response_payload_limit(4 * 1024 * 1024)
            .set_h1_connection_limit(32)
            .set_header("user-agent", "my-service/1.0")?
            .set_bearer_auth(token)?,
    );
    Ok(Client::with_config(cfg))
}
```

Here is the full list, with defaults:

| Setting | Default | Meaning |
|---|---|---|
| `set_response_timeout` | 5 s | Time to wait for the response head after the request is sent; `disable_timeout()` sets it to zero |
| `set_response_payload_limit` | 256 KiB | Largest body that `body()` and `json()` will buffer; 0 means no limit |
| `set_response_payload_timeout` | 10 s | Time allowed for `body()` and `json()` to read the whole body; independent of the response timeout |
| `set_h1_connection_limit` | 8 | HTTP/1.1 connections per pool, shared by all hosts |
| `set_h1_keepalive` | 15 s | How long an idle HTTP/1.1 connection may wait in the pool |
| `set_h1_lifetime` | 75 s | Maximum age of a pooled HTTP/1.1 connection |
| `set_h2_connection_limit` | 16 | HTTP/2 connections per host |
| `set_h2_max_streams` | 100 | Concurrent requests per HTTP/2 connection |
| `set_h2_keepalive` | 60 s | How long an idle HTTP/2 connection is kept |
| `set_h2_lifetime` | 1 h | Maximum age of an HTTP/2 connection |

Connection limits don't make requests fail. When every connection is busy,
new requests simply wait their turn. The plain and TLS pools count their
connections separately.

`set_header`, `set_basic_auth`, and `set_bearer_auth` add headers to every
request the client sends. If a request sets a header with the same name, the
request wins. These methods return a `Result` because a header name or value
can be invalid, which is why the example uses `?`.

### I/O settings

Client connections are ordinary ntex I/O objects, so everything in
[`IoConfig`] from the [I/O chapter](7-io.md) applies to them. The setting
you are most likely to need is the connect timeout. It is off by default,
which means a request to an unreachable host waits for as long as the
operating system is willing to try. In production, set one:

```rust
use ntex::client::{Client, ClientConfig};
use ntex::io::IoConfig;
use ntex::{SharedCfg, time::Seconds};

fn client() -> Client {
    let cfg = SharedCfg::new("API-CLIENT")
        .add(ClientConfig::new().set_response_timeout(Seconds(10)))
        .add(IoConfig::new().set_connect_timeout(Seconds(3)));
    Client::with_config(cfg)
}
```

A request moves through several stages. Each timeout covers exactly one of
them, and turning one off leaves the others in place:

1. **Waiting for a pooled connection** when the connection limit is reached:
   no timeout. Wrap `send()` in `ntex::time::timeout` if you need one.
2. **Connecting**, including the DNS lookup:
   `IoConfig::set_connect_timeout`, off by default.
3. **The TLS handshake**: `TlsConfig::set_handshake_timeout`, 5 seconds.
4. **Sending the request body**: no client timeout. For HTTP/1.1,
   `IoConfig::set_write_timeout`, off by default, bounds each wait for the
   socket to accept more data, but not a slow body stream.
5. **Waiting for the response head**: `ClientConfig::set_response_timeout`,
   5 seconds, or `timeout()` on an individual request.
6. **Reading the body** with `body()` or `json()`:
   `ClientConfig::set_response_payload_timeout`, 10 seconds, or `timeout()`
   on the body future. Reading the response as a stream has no timeout.

So `ClientConfig::disable_timeout()` and `timeout(Millis::ZERO)` only lift
the limit on waiting for the response head. A slow body still fails after the payload
timeout unless you also set `set_response_payload_timeout(Millis::ZERO)`.
There is no single deadline for the whole request; for that, wrap the whole
exchange, `send()` plus reading the body, in `ntex::time::timeout`.

### Secure connections

The client keeps two connectors, one for each kind of URL. `http` URLs go
through the plain connector, which opens a TCP connection. `https` URLs go
through the secure connector, which opens a TCP connection and then performs
the TLS handshake on it. The builder lets you replace each one separately:
`connector()` sets the plain connector and `secure_connector()` the secure
one.

You rarely need to set the secure connector by hand. TLS comes from the
`openssl` or `rustls` feature of `ntex`. Enable one, and `Client::new()` and
`Client::builder()` install a secure connector for you. What that connector
does depends on the backend:

- **rustls** checks server certificates against the webpki root
  certificates, as you would expect.
- **OpenSSL** checks server certificates against OpenSSL's default trust
  store (the system roots) and checks that the certificate matches the
  request host. To trust a private root, or to accept a self-signed
  certificate while testing, give the client your own configuration.

If both features are enabled, OpenSSL wins. If neither is, `https` requests
fail with `ConnectError::SslIsNotSupported`.

To use your own TLS configuration, hand it to the builder; `.rustls()` and
`.openssl()` build a secure connector from it. The builder uses
it exactly as given, so if you want HTTP/2, set the ALPN protocols yourself:

```rust
use ntex::{SharedCfg, client::Client};
use rustls::{ClientConfig, RootCertStore};

fn https_client() -> Client {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Client::builder().rustls(tls).build(SharedCfg::new("HTTPS-CLIENT"))
}
```

OpenSSL works the same way: build an `openssl::ssl::SslConnector`, set ALPN
with `set_alpn_protos(b"\x02h2\x08http/1.1")`, and pass it to `.openssl()`.

`.rustls()` and `.openssl()` are shortcuts. Each wraps your configuration in
one of ntex's TLS connectors, `ntex::connect::rustls::TlsConnector` or
`ntex::connect::openssl::SslConnector`, and passes it to
`secure_connector()`. You can call `secure_connector()` directly with any
connector you like. It might be an OpenSSL or rustls connector set up
differently, or one built on another TLS implementation entirely, such as
Schannel on Windows or Secure Transport on macOS. The client doesn't care how
the connection is secured. A connector is a service that takes a
`ntex::connect::Connect` holding the target URL and returns an I/O object.
If that I/O object reports HTTP/2 as its negotiated protocol, the client
speaks HTTP/2 over it.

`connector()` takes the same kind of service for plain `http` URLs. Use it
when connections need something other than a direct TCP connection, for
example a tunnel through a proxy or a custom transport.

For certificate verification, private roots, and client certificates, see
the [TLS chapter](10-tls.md#http-client).

## Sending requests

`Client` has a method for each common HTTP verb: `get`, `post`, `put`,
`patch`, `delete`, `head`, `options`, and `query`. For anything else there is
`request(method, url)`. Each returns a [`ClientRequest`] builder.

URLs must be absolute: an `http` or `https` scheme plus a host.
You may notice that the builder methods never return errors. If something is
wrong, such as a bad URL, an invalid header, or a query that won't
serialize, the builder remembers the error and returns it when you send the
request. That keeps the builder chain free of `?`.

### Building the request

Here is a request that uses a few of the builder's features:

```rust,no_run
use serde::Serialize;
use ntex::client::Client;
use ntex::time::Seconds;

#[derive(Serialize)]
struct Search<'a> {
    q: &'a str,
    page: u32,
}

async fn search(client: &Client, token: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let response = client
        .get("https://api.example.com/search")
        .query(&Search { q: "ntex", page: 2 })
        .header("accept", "application/json")
        .if_some(token, |token, req| req.bearer_auth(token))
        .timeout(Seconds(2))
        .send()
        .await?;

    println!("{}", response.status());
    Ok(())
}
```

`query()` serializes any `serde::Serialize` value into the query string,
replacing whatever was there. `if_some()` applies a change only when an
`Option` has a value, here adding a token only when there is one. Its sibling
`if_true()` does the same for a `bool`. Both let you make a request
conditional without breaking up the chain. `timeout()` overrides the
client's response-head timeout for this one request; a zero duration turns
that timeout off, but not the payload timeout or any connect timeout.

For headers you have three choices. `header()` appends a value,
`set_header()` replaces any existing value, and `set_header_if_none()` sets
the header only if it isn't there yet. `content_type()` and
`content_length()` are shortcuts for the two most common ones. For
authentication there are `basic_auth(user, password)` and
`bearer_auth(token)`, and with the `cookie` feature, `cookie()` attaches a
cookie.

A few methods control the connection itself. `force_close()` closes an
HTTP/1.1 connection after the response instead of returning it to the pool.
`address()` connects to a specific socket address and skips DNS. The URL
still decides the `Host` header and the TLS server name, which makes this
useful for testing a particular backend behind a load balancer.

With the `compress` feature, the client asks servers for compressed
responses and decompresses them for you. `no_decompress()` turns that off for
a single request, which is usually what a proxy wants. Proxies will also like
`client.request_from(url, head)`, which starts a request with the method and
headers copied from an incoming one.

### Sending a body

You choose how to send the body by choosing which `send` method to call:

| Method | Body |
|---|---|
| `send()` | No body |
| `send_body(body)` | Anything that converts to `Body`: `&'static str`, `String`, `Bytes`, `Vec<u8>`, ... |
| `send_json(&value)` | `value` serialized as JSON; sets `Content-Type: application/json` unless you set one |
| `send_form(&value)` | `value` serialized as a URL-encoded form; sets the matching content type |
| `send_stream(stream)` | A stream of `Result<Bytes, E>` chunks, sent as they arrive |

The first four cover most needs:

```rust,no_run
use serde::Serialize;
use ntex::client::Client;

#[derive(Serialize)]
struct NewUser<'a> {
    name: &'a str,
    email: &'a str,
}

async fn send_bodies(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    // Raw bytes or text
    client.post("http://127.0.0.1:8080/notes")
        .content_type("text/plain")
        .send_body("remember the milk")
        .await?;

    // JSON
    let user = NewUser { name: "ferris", email: "ferris@example.com" };
    client.post("http://127.0.0.1:8080/users").send_json(&user).await?;

    // A URL-encoded form
    client.post("http://127.0.0.1:8080/login")
        .send_form(&[("user", "ferris"), ("password", "secret")])
        .await?;
    Ok(())
}
```

`send_stream()` is for bodies that are large, or that are produced while the
request is already underway. The body never has to sit in memory all at
once. Any `Stream` of `Result<Bytes, E>` will do. In this example, a separate
task produces the chunks and passes them through a channel:

```rust,no_run
use ntex::{channel::mpsc, client::Client, util::Bytes};

async fn upload(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>();

    ntex::rt::spawn(async move {
        for i in 0..10 {
            let chunk = Bytes::from(format!("line {i}\n"));
            if tx.send(Ok(chunk)).is_err() {
                break; // the request was dropped
            }
        }
    });

    let response = client
        .post("http://127.0.0.1:8080/upload")
        .send_stream(rx)
        .await?;
    println!("{}", response.status());
    Ok(())
}
```

Because the total length isn't known up front, HTTP/1.1 sends the body with
chunked transfer encoding. If the stream yields an error, the request fails.

### When something goes wrong

Every `send` method returns `Result<ClientResponse, Error<ClientError>>`.
`Error` dereferences to [`ClientError`], so you can match on what went wrong
and react accordingly:

```rust,no_run
use ntex::client::{Client, error::ClientError};

async fn fetch(client: &Client) {
    match client.get("http://127.0.0.1:8080/").send().await {
        Ok(response) => println!("status: {}", response.status()),
        Err(err) => match &*err {
            ClientError::Timeout => eprintln!("the server took too long to answer"),
            ClientError::Connect(e) => eprintln!("could not connect: {e}"),
            _ => eprintln!("request failed: {err}"),
        },
    }
}
```

Note that a 404 or a 500 is not an error here. From the client's point of
view, the request worked: the server received it and answered. It's up to
you to check `response.status()` and decide what that answer means.

## Reading responses

`send()` completes as soon as the response head arrives. You can look at the
status and headers right away, while the body may still be on its way.

```rust,no_run
use ntex::client::ClientResponse;
use ntex::http::{StatusCode, header};

fn inspect(response: &ClientResponse) {
    if response.status() == StatusCode::NOT_FOUND {
        println!("no such resource");
    }
    println!("version: {:?}", response.version());
    if let Some(value) = response.header(header::CONTENT_TYPE) {
        println!("content type: {value:?}");
    }
    for (name, value) in response.headers() {
        println!("{name}: {value:?}");
    }
}
```

`ClientResponse` also implements `HttpMessage`, which brings helpers such as
`content_type()` and, with the `cookie` feature, `cookies()`.

How you read the body depends on how big it is and what you plan to do with
it.

### Buffering the body

If the body is reasonably small, read it all at once. `body()` collects it
into `Bytes`, and `json::<T>()` deserializes it with serde (it refuses
responses whose content type isn't JSON). Both enforce the payload limit and
payload timeout from `ClientConfig`, and you can adjust either for a single
response:

```rust,no_run
use serde::Deserialize;
use ntex::client::Client;
use ntex::time::Seconds;

#[derive(Deserialize)]
struct User {
    id: u64,
    name: String,
}

async fn read(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    // Up to 10 MB, within 30 seconds
    let response = client.get("http://127.0.0.1:8080/report.csv").send().await?;
    let report = response.body().limit(10 * 1024 * 1024).timeout(Seconds(30)).await?;
    println!("report: {} bytes", report.len());

    // A JSON document
    let response = client.get("http://127.0.0.1:8080/users/1").send().await?;
    let user: User = response.json().await?;
    println!("user {}: {}", user.id, user.name);
    Ok(())
}
```

The limit is there to protect you. A misbehaving server could otherwise send
you gigabytes, and you'd buffer every byte. If the response declares a
`Content-Length` above the limit, the read fails right away without reading
any of the body.

The body can be read only once. `body()` and `json()` take the payload as
soon as you call them, not when the future first runs, so dropping the future
also drops the body. Any later `body()`, `json()` or stream read fails with
`PayloadError::AlreadyTaken` instead of silently returning nothing. A
response that never had a body still reads as empty bytes.

`json()` checks the content type before taking the payload, so after a
`ContentType` error you can still read the body. Once the JSON fails to
parse, though, the bytes are gone. If you need the raw body on failure, for
example to log an error page, buffer it once and parse it yourself:

```rust,no_run
use serde::Deserialize;
use ntex::client::Client;

#[derive(Deserialize)]
struct User {
    id: u64,
}

async fn read(client: &Client) -> Result<(), Box<dyn std::error::Error>> {
    let response = client.get("http://127.0.0.1:8080/users/1").send().await?;
    let body = response.body().await?;
    match serde_json::from_slice::<User>(&body) {
        Ok(user) => println!("user {}", user.id),
        Err(e) => println!("bad response ({e}): {}", String::from_utf8_lossy(&body)),
    }
    Ok(())
}
```

### Streaming the body

For large downloads, or bodies with no fixed end, read the response chunk by
chunk. `ClientResponse` is itself a `Stream`, and `ntex::util::stream_recv`
waits for the next chunk:

```rust,no_run
use ntex::{client::Client, util::stream_recv};

async fn download(client: &Client) -> Result<usize, Box<dyn std::error::Error>> {
    let mut response = client.get("http://127.0.0.1:8080/large-file").send().await?;

    let mut total = 0;
    while let Some(chunk) = stream_recv(&mut response).await {
        let chunk = chunk?;
        total += chunk.len(); // write the chunk to a file, a socket, ...
    }
    Ok(total)
}
```

Since you only hold one chunk at a time, the payload limit and payload
timeout don't apply here. If you need them, enforce them in the loop. To pass
the body on to something else, such as a response in a proxy, take it with
`take_payload()`. If the body was already taken, the stream yields a single
`PayloadError::AlreadyTaken` error and then ends; `set_payload()` installs a
new body and makes the response readable again.

## Middleware

Middleware wraps the client's service chain. Each request passes through it
on the way out, and each response on the way back. It is the same idea as
[web middleware](8.3-web-app.md#middleware), pointed in the other direction.

### Why use middleware

Some things should happen on every request, no matter where in your code the
request is made. Without middleware, you'd have to remember them at every
call site. With middleware, you write them once:

- **Authentication** that changes over time, such as an access token that a
  background task refreshes.
- **Tracing**: adding a request ID or trace context to every call.
- **Logging and metrics**: recording method, URL, status, and latency.
- **Policy**: refusing to call hosts the service shouldn't talk to.
- **Rewriting**: sending requests to a different host in tests, or adjusting
  response headers.

You don't need middleware for headers that never change, such as a fixed
`User-Agent` or a static API key. The default headers in `ClientConfig`
handle those.

### How middleware works

Middleware is installed with `ClientBuilder::middleware()`. The quickest way
to write one is `fn_layer` from `ntex::service`, which turns an async closure
into middleware. The closure gets the request and the next service in the
chain, and returns whatever that service returns. This one adds an
`Authorization` header, reading the token fresh for each request so that a
refreshed token is picked up immediately:

```rust,no_run
use std::{cell::RefCell, rc::Rc};

use ntex::client::{Client, ServiceRequest};
use ntex::http::header::{AUTHORIZATION, HeaderValue};
use ntex::{SharedCfg, service::fn_layer};

fn client(token: Rc<RefCell<HeaderValue>>) -> Client {
    Client::builder()
        .middleware(fn_layer(async move |mut req: ServiceRequest, svc| {
            // Read the current token for every request
            let value = token.borrow().clone();
            req.headers().insert(AUTHORIZATION, value);
            svc.call(req).await
        }))
        .build(SharedCfg::new("API-CLIENT"))
}
```

Middleware doesn't see the friendly `ClientRequest` builder. It works with a
[`ServiceRequest`], the request as it is about to be sent. `head()` gives you
the method, URL, and headers. `headers()` is a separate set of headers that
take precedence over the ones in the head, which makes it a convenient place
to add or override a header. `body()` and `address()` give you the body and
the target address.

On the way back, middleware gets a [`ServiceResponse`]: the response before
it becomes a `ClientResponse`. `head()` holds the status and headers, and
`payload()` the body stream. Middleware sees the response as soon as its head
arrives, before the body is read, so timing a request in middleware measures
the time until the response head arrives, not the full download.

Middleware can stop a request by returning an error instead of calling the
next service. It can't invent a response, though; every successful response
comes from a real server.

You can stack several middleware. The first one you add is the outermost:
it sees the request first and the response last.

For middleware you want to reuse, or that needs its own readiness or
shutdown logic, implement the [`Middleware`] trait and a service, just as you
would on a server. The only differences are the types: the configuration is
`SharedCfg` and the request is `ServiceRequest`. Here is middleware that logs
how each request went:

```rust
use std::time::Instant;

use ntex::client::{ServiceRequest, ServiceResponse, error::ClientError};
use ntex::{Ctx, Middleware, Service, SharedCfg, error::Error};

/// Logs the method, URL, status, and latency of every request.
pub struct Timing;

pub struct TimingService<S> {
    service: S,
}

impl<S> Middleware<S, SharedCfg> for Timing {
    type Service = TimingService<S>;

    fn create(&self, _: &SharedCfg, service: S) -> Self::Service {
        TimingService { service }
    }
}

impl<S> Service<SharedCfg, ServiceRequest> for TimingService<S>
where
    S: Service<SharedCfg, ServiceRequest, Res = ServiceResponse, Error = Error<ClientError>>,
{
    type Res = ServiceResponse;
    type Error = Error<ClientError>;

    ntex::forward!(SharedCfg, service);

    async fn call(
        &self,
        mut req: ServiceRequest,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<ServiceResponse, Self::Error> {
        let method = req.head().method.clone();
        let url = req.head().uri.to_string();
        let start = Instant::now();

        let mut result = ctx.call(&self.service, req).await;
        match &mut result {
            Ok(res) => log::info!("{method} {url} -> {} in {:?}", res.head().status, start.elapsed()),
            Err(err) => log::warn!("{method} {url} failed after {:?}: {err}", start.elapsed()),
        }
        result
    }
}
```

Installing it looks just like the closure version:

```rust
# use ntex::client::{ServiceRequest, ServiceResponse, error::ClientError};
# use ntex::{Ctx, Middleware, Service, SharedCfg, error::Error};
# pub struct Timing;
# pub struct TimingService<S> { service: S }
# impl<S> Middleware<S, SharedCfg> for Timing {
#     type Service = TimingService<S>;
#     fn create(&self, _: &SharedCfg, service: S) -> Self::Service { TimingService { service } }
# }
# impl<S> Service<SharedCfg, ServiceRequest> for TimingService<S>
# where S: Service<SharedCfg, ServiceRequest, Res = ServiceResponse, Error = Error<ClientError>>,
# {
#     type Res = ServiceResponse;
#     type Error = Error<ClientError>;
#     ntex::forward!(SharedCfg, service);
#     async fn call(&self, req: ServiceRequest, ctx: Ctx<'_, Self, SharedCfg>) -> Result<ServiceResponse, Self::Error> {
#         ctx.call(&self.service, req).await
#     }
# }
let client = ntex::client::Client::builder()
    .middleware(Timing)
    .build(SharedCfg::new("API-CLIENT"));
```

A word of caution about retries. It is tempting to write middleware that
resends a failed request, but a request is consumed when it is sent, and a
streaming body can only be sent once. There is no copy of `ServiceRequest`
left to resend. Retries are usually easier in application code, where you can
simply build the request again.

## Summary

- Create one `Client` per kind of outgoing traffic, once per thread, and
  reuse it for the life of the application. Clones are cheap and share
  connection pools.
- Tune the client with `ClientConfig`, `IoConfig`, and friends in a
  `SharedCfg`. Always set a connect timeout.
- The default secure connector verifies certificates. Provide your own TLS
  configuration for private roots or testing, and set ALPN on it if you want
  HTTP/2.
- Pick the body format by picking the method: `send`, `send_body`,
  `send_json`, `send_form`, or `send_stream`.
- Buffer small bodies with `body()` or `json()`, and stream large ones.
  Read bodies to the end so connections can be reused.
- Reach for middleware when something must happen on every request:
  authentication, tracing, logging, metrics, or policy.

To see a complete program, look at the [HTTP client example] and the
[HTTP proxy example].

[`SharedCfg`]: https://docs.ntex.rs/api/ntex/service/cfg/struct.SharedCfg.html
[`ClientConfig`]: https://docs.ntex.rs/api/ntex/client/struct.ClientConfig.html
[`IoConfig`]: https://docs.ntex.rs/api/ntex/io/struct.IoConfig.html
[`TlsConfig`]: https://docs.ntex.rs/api/ntex/tls/struct.TlsConfig.html
[`HttpServiceConfig`]: https://docs.ntex.rs/api/ntex/http/struct.HttpServiceConfig.html
[`Http2ServiceConfig`]: https://docs.ntex.rs/api/ntex/http/h2/struct.Http2ServiceConfig.html
[`ClientRequest`]: https://docs.ntex.rs/api/ntex/client/struct.ClientRequest.html
[`ClientError`]: https://docs.ntex.rs/api/ntex/client/error/enum.ClientError.html
[`ServiceRequest`]: https://docs.ntex.rs/api/ntex/client/struct.ServiceRequest.html
[`ServiceResponse`]: https://docs.ntex.rs/api/ntex/client/struct.ServiceResponse.html
[`Middleware`]: https://docs.ntex.rs/api/ntex/service/trait.Middleware.html
[HTTP client example]: https://github.com/ntex-rs/examples/tree/main/extras/http-client
[HTTP proxy example]: https://github.com/ntex-rs/examples/tree/main/extras/http-proxy
