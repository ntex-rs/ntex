# Cookbook

These recipes are small applications you can copy into `src/main.rs`. Each
runs independently; stop one before starting another on port 8080. They use
ntex 4.7 with the `tokio` feature. The JSON recipes also need `serde` with
`derive` and `serde_json`; the logging recipe needs `env_logger`. Static files
and compression list their additional requirements beside the example.

The examples keep error responses short so you can see the wiring. A real
application should log internal errors without returning credentials, database
errors, or upstream response details to clients. See
[Error Handling](./8.5-web-app.md) for application-specific errors and
[Testing](./testing.md) for checking these applications without fixed ports.

## A JSON API with shared in-memory state

Use this when you need a small REST API before introducing a database. This
example stores notes by numeric ID. `POST` creates a note, `GET` reads it,
`PUT` updates an existing note, and `DELETE` removes it. All workers see the
same map because the application creates one `Arc<Mutex<_>>` before starting
the server, then clones that handle into each worker's application.

```rust
use ntex::web::{
    HttpResponse,
    types::{Json, Path},
};
use ntex::{SharedCfg, web};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Deserialize, Serialize)]
struct Note {
    text: String,
}

type Notes = web::AppState<Arc<Mutex<BTreeMap<u64, Note>>>>;

async fn create(state: &Notes, (): (), id: Path<u64>, note: Json<Note>) -> HttpResponse {
    let Ok(mut notes) = state.lock() else {
        return HttpResponse::InternalServerError().build();
    };
    if notes.contains_key(&id) {
        return HttpResponse::Conflict().build();
    }
    let note = note.into_inner();
    notes.insert(id.into_inner(), note.clone());
    HttpResponse::Created().json(&note)
}

async fn read(state: &Notes, (): (), id: Path<u64>) -> HttpResponse {
    let Ok(notes) = state.lock() else {
        return HttpResponse::InternalServerError().build();
    };
    match notes.get(&id) {
        Some(note) => HttpResponse::Ok().json(note),
        None => HttpResponse::NotFound().build(),
    }
}

async fn update(state: &Notes, (): (), id: Path<u64>, note: Json<Note>) -> HttpResponse {
    let Ok(mut notes) = state.lock() else {
        return HttpResponse::InternalServerError().build();
    };
    let Some(current) = notes.get_mut(&id) else {
        return HttpResponse::NotFound().build();
    };
    *current = note.into_inner();
    HttpResponse::Ok().json(current)
}

async fn delete(state: &Notes, (): (), id: Path<u64>) -> HttpResponse {
    let Ok(mut notes) = state.lock() else {
        return HttpResponse::InternalServerError().build();
    };
    if notes.remove(&id).is_some() {
        HttpResponse::NoContent().build()
    } else {
        HttpResponse::NotFound().build()
    }
}

fn routes(cfg: &mut web::ServiceConfig<Notes>) {
    cfg.service(
        web::resource("/notes/{id}")
            .route(web::post().to_with_state(create))
            .route(web::get().to_with_state(read))
            .route(web::put().to_with_state(update))
            .route(web::delete().to_with_state(delete)),
    );
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let notes = Arc::new(Mutex::new(BTreeMap::new()));
    web::HttpServer::new(async move |_| {
        web::App::<Notes>::new()
            .configure(routes)
            .build_with(web::AppState::new(notes.clone()))
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Send `application/json`, for example `{"text":"Buy bread"}`, to
`POST /notes/1`. Creating that ID twice returns 409; reading, updating, or
removing a missing ID returns 404. The mutex keeps the existence check and
insertion together, so two workers cannot both create the same ID. A
poisoned lock produces 500 instead of panicking on another request.

These handlers do no asynchronous work while holding the lock. Keep critical
sections short: a contended standard mutex blocks the worker thread. For a
worker-local cache, construct an `Rc<RefCell<_>>` inside the worker factory
instead, but then each worker has different contents. Neither choice makes
the data persistent across process restarts. See
[Application State](./8.2-web-app.md) and
[Worker and Request State](./6-server-app.md).

## Serving a directory of static files

Use this for a small site's assets or downloads that live in a dedicated
public directory. Directory serving comes from the separate `ntex-files`
crate, not `ntex::web` itself. Add `ntex-files = "4.4"` alongside ntex. Create a
`public` directory containing `index.html` before starting this application;
requests under `/static` are resolved inside that directory.

```rust
use ntex::{SharedCfg, web};
use ntex_files::Files;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let public = std::fs::canonicalize("./public")?;
    web::HttpServer::new(async move |_| {
        web::App::new().service(Files::new("/static", public.clone()).index_file("index.html"))
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Directory listings are off by default. `index_file` selects a document to
serve for directory requests; it does not enable listing. `Files` supports
`GET` and `HEAD`, conditional requests, and byte ranges. It checks decoded
path segments and confines resolved paths to its configured base directory
unless you explicitly enable following paths outside it with
`follow_symlinks(true)`. Keep that option off for public downloads.

Treat the public directory as deployment-owned, not writable by untrusted
users. Filesystem validation cannot make arbitrary concurrent changes to the
directory safe. Do not mount your project root just to expose a few assets:
non-hidden configuration files and source files would also become public.

If you write a file handler instead, never join an unchecked URL onto a disk
path. An ordinary `Path<String>` segment is percent-decoded and can contain
`/`; a `{tail}*` remainder stays encoded and needs explicit decoding before
validation. Reject traversal and platform-specific separators, and enforce
containment after resolving filesystem paths. See
[Extractors and Responders](./8.4-web-app.md) for path extraction and
[Routing and Handlers](./8.1-web-app.md) for mounting services.

## Rejecting requests without an API key

Use this for a simple service-to-service endpoint whose callers share an API
key. Set `API_KEY` in the process environment and send its value in the
`x-api-key` request header. The middleware checks the key before calling the
handler, so rejected requests receive 401 without reaching protected code.
Use HTTPS when the key crosses a network you do not trust.

```rust
use ntex::web::{HttpResponse, middleware::from_fn};
use ntex::{SharedCfg, web};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let key = std::env::var("API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .expect("set a nonempty API_KEY");
    web::HttpServer::new(async move |_| {
        let key = key.clone();
        web::App::new()
            .middleware(from_fn(async move |req, next| {
                let mut values = req.headers().get_all("x-api-key");
                let allowed = values
                    .next()
                    .is_some_and(|v| v.as_bytes() == key.as_bytes())
                    && values.next().is_none();
                if !allowed {
                    return Ok(
                        req.into_response(HttpResponse::Unauthorized().body("invalid API key"))
                    );
                }
                next.call(req).await
            }))
            .route("/private", web::get().to(async || "allowed"))
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

The comparison is exact and rejects missing, incorrect, or duplicate keys.
It is an intentionally small example, not a credential-management system.
For production secrets, use a reviewed authentication implementation with
appropriate constant-time comparison, rotation, and rate limiting. Do not
log the key. An API key identifies its holder; authorization rules still
need to decide what that holder may do.

Attach middleware before adding routes. If only one URL subtree needs a key,
put it on a scope rather than the whole application. Returning
`req.into_response(...)` is the normal early-response path; successful
requests must call `next.call(req).await`. See
[Filters and Middleware](./8.3-web-app.md),
[Error Handling](./8.5-web-app.md), and [TLS](./10-tls.md).

## Logging requests and their duration

Use this when you want an access log with status codes, response sizes, and
elapsed time. ntex includes `Logger`, so you do not need to implement timing
middleware for the usual access-log fields. Initialize the logging backend
once in `main`, not in the worker factory, and run with `RUST_LOG=info`.

```rust
use ntex::web::middleware::Logger;
use ntex::{SharedCfg, web};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    web::HttpServer::new(async |_| {
        web::App::new()
            .middleware(Logger::new("%U %s %b %Dms").exclude("/health"))
            .route("/health", web::get().to(async || "ok"))
            .route("/hello", web::get().to(async || "hello"))
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

`%U` logs the path without the query string, `%s` the response status, `%b`
the body bytes, and `%D` elapsed milliseconds. Logger records the response
when its wrapped body is dropped, rather than only when the handler returns.
That distinction matters for streaming responses and clients that disconnect.
Excluding a health endpoint keeps frequent probes out of the access log.

Avoid logging authorization headers, cookies, or sensitive query parameters.
For a custom metric around handler execution, use `from_fn`, start an
`Instant` before `next.call`, and observe the result afterward; that measures
a different interval from consuming the response body. See
[Filters and Middleware](./8.3-web-app.md).

## Graceful shutdown, including Ctrl-C

Use this when Ctrl-C should drain active requests instead of stopping
immediately. ntex normally stops gracefully on `SIGTERM`, but immediately
on `SIGINT`/Ctrl-C; `graceful_shutdown()` does not change `SIGINT`. This
example disables the server's own signal reaction, enables runtime signal
handling, and translates interrupt, termination, or quit into `stop(true)`.

```rust
use ntex::rt::signals::{self, Signal};
use ntex::{SharedCfg, rt, time::Seconds, web};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    rt::System::current().enable_signals();
    let server = web::HttpServer::new(async |_| {
        web::App::new().route("/", web::get().to(async || "running"))
    })
    .disable_signals()
    .graceful_shutdown_timeout(Seconds(20))
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run();

    let handle = server.clone();
    let mut incoming = signals::signal();
    rt::spawn(async move {
        while let Ok(batch) = incoming.await {
            if batch
                .iter()
                .any(|signal| matches!(signal, Signal::Int | Signal::Term | Signal::Quit))
            {
                handle.stop(true).await;
                break;
            }
            incoming = signals::signal();
        }
    });
    server.await
}
```

Signal subscriptions receive one batch, so register again after an unrelated
signal. Windows delivers Ctrl-C as `Signal::Int`; Unix also supplies the
termination and quit signals shown here. Only one ntex system can own
process signal handling at a time. If an embedding application already owns
signals, let it call a cloned server handle instead of installing another
handler.

`run()` starts the server and returns its controller/completion future.
`stop(true).await` requests graceful worker shutdown; `stop(false)` requests
an immediate stop. The worker deadline above bounds draining, so requests
still active after it can be dropped. It is not a guarantee that every
request completes, nor a timeout for individual handlers. See
[Server](./5-server.md#signals-and-shutdown) and
[Worker and Request State](./6-server-app.md) for lifecycle details.

## Body limits and separate timeout budgets

Use this when endpoints accept small JSON documents but you also need to
bound slow clients and slow application work. These are separate controls:
extractor configuration limits a buffered body, HTTP configuration controls
connection behavior, and `time::timeout` bounds the future you wrap. This
example caps JSON at 1 KiB and raw byte bodies at 4 KiB.

```rust
use ntex::web::{
    HttpResponse, WebAppConfig,
    types::{Json, JsonConfig, PayloadConfig},
};
use ntex::{
    SharedCfg,
    http::HttpServiceConfig,
    time::{self, Seconds},
    web,
};

async fn json(body: Json<serde_json::Value>) -> HttpResponse {
    HttpResponse::Ok().json(&body.into_inner())
}

async fn raw(body: ntex::util::Bytes) -> String {
    format!("{} bytes", body.len())
}

async fn bounded() -> HttpResponse {
    let work = async { "finished" };
    match time::timeout(Seconds(2), work).await {
        Ok(result) => HttpResponse::Ok().body(result),
        Err(_) => HttpResponse::GatewayTimeout().build(),
    }
}

fn limits() -> WebAppConfig {
    WebAppConfig::new()
        .set_value(JsonConfig::default().limit(1024))
        .set_value(PayloadConfig::new(4096))
}

fn routes(cfg: &mut web::ServiceConfig) {
    cfg.route("/json", web::post().to(json));
    cfg.route("/raw", web::post().to(raw));
    cfg.route("/work", web::get().to(bounded));
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let transport = SharedCfg::new("bounded-api")
        .add(
            HttpServiceConfig::new()
                .set_client_timeout(Seconds(3))
                .set_keepalive_timeout(Seconds(10))
                .set_write_timeout(Seconds(15)),
        )
        .build();
    web::HttpServer::new(async |_| web::App::new().with_config(limits()).configure(routes))
        .bind("127.0.0.1:8080", transport)?
        .run()
        .await
}
```

An oversized JSON or byte body returns 413 with the default error domain.
`JsonConfig` does not configure the `Bytes` extractor; `PayloadConfig` does.
Neither is a universal streaming-body cap: code consuming `Payload` directly
must enforce its own bound. Store these settings in `WebAppConfig`, not
application state. They are extractor settings, separate from `SharedCfg`.

`set_client_timeout` sets the initial request-header timeout and the
header-read rate's measurement interval, not a handler deadline or a total
body-read deadline. `set_keepalive_timeout` controls idle persistent
connections. `set_write_timeout` bounds HTTP/1 write backpressure. HTTP/1
protocol timers have one-second resolution, so allow for that when testing.
For slow uploads, HTTP configuration also has `set_payload_read_rate`; choose
its rate and budget for your expected traffic rather than treating the header
timeout as upload protection.

Replace `work` with the operation you want to bound. A timeout drops its
future; it does not roll back external side effects or interrupt blocking
code. Wrapping work inside the handler also does not time the extraction
that happened before entering it. See
[Extractors and Responders](./8.4-web-app.md),
[Server Configuration](./5-server.md#connection-and-protocol-configuration),
and [Timers](./12-time.md).

## Calling another HTTP service

Use this when a handler needs data from another HTTP endpoint. Build one
`ntex::client::Client` per worker, then clone its handle with application
state so connections can reuse its pool. The upstream URL here is fixed;
set `UPSTREAM_URL` to a trusted service address rather than accepting an
arbitrary destination from the request.

```rust
use ntex::web::HttpResponse;
use ntex::{SharedCfg, client::Client, time::Seconds, web};

#[derive(Clone)]
struct Upstream {
    client: Client,
    url: String,
}
type State = web::AppState<Upstream>;

async fn fetch(state: &State, (): ()) -> HttpResponse {
    let response = match state
        .client
        .get(state.url.as_str())
        .timeout(Seconds(2))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        _ => return HttpResponse::BadGateway().build(),
    };
    match response.body().limit(64 * 1024).timeout(Seconds(2)).await {
        Ok(body) => HttpResponse::Ok().content_type("text/plain").body(body),
        Err(_) => HttpResponse::BadGateway().build(),
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let url = std::env::var("UPSTREAM_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:9000/message".to_owned());
    web::HttpServer::new(async move |_| {
        let state = web::AppState::new(Upstream {
            client: Client::new(),
            url: url.clone(),
        });
        web::App::<State>::new()
            .route("/message", web::get().to_with_state(fetch))
            .build_with(state)
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

The client lives in the worker factory, not inside `fetch` or a per-connection
service constructor. Its clones share connection pools and are worker-local;
do not move this client between worker threads. The request timeout and body
read timeout are separate budgets. Reading the full body allows the client
to reuse a healthy connection; the limit keeps buffering bounded.

This is not a transparent proxy. It accepts successful upstream responses,
returns plain text, and maps transport errors and other statuses to 502.
Do not blindly copy hop-by-hop headers or let callers choose internal URLs.
HTTPS needs an enabled TLS backend or a configured secure connector. See
[HTTP Client](./13-client.md) and [TLS](./10-tls.md).

## Keeping a database pool in state

Use this pattern when a handler needs a database pool but you do not want
routing code tied to one database library. The small trait below is an
application boundary: implement it for a wrapper around your chosen pool.
`MemoryPool` is only an in-memory stand-in so the example compiles without a
database dependency; it does not open connections or implement a real pool.

```rust
use ntex::web::{HttpResponse, types::Path};
use ntex::{SharedCfg, web};
use std::{collections::BTreeMap, rc::Rc};

trait UserPool: Clone + 'static {
    async fn name(&self, id: u64) -> Option<String>;
}

#[derive(Clone)]
struct MemoryPool(Rc<BTreeMap<u64, String>>);

impl UserPool for MemoryPool {
    async fn name(&self, id: u64) -> Option<String> {
        self.0.get(&id).cloned()
    }
}

#[derive(Clone)]
struct Database<P> {
    pool: P,
}

impl<P: UserPool> web::State for Database<P> {
    type Error = web::DefaultError;
}

async fn user<P: UserPool>(state: &Database<P>, (): (), id: Path<u64>) -> HttpResponse {
    match state.pool.name(id.into_inner()).await {
        Some(name) => HttpResponse::Ok().json(&name),
        None => HttpResponse::NotFound().build(),
    }
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::with_config(
        async || {
            let pool = MemoryPool(Rc::new(BTreeMap::from([(1, "Ada".to_owned())])));
            Ok(Database { pool })
        },
        async |_| {
            web::App::new().route("/users/{id}", web::get().to_with_state(user::<MemoryPool>))
        },
    )
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Replace construction in the zero-argument configuration factory with your
pool's asynchronous initialization. That factory runs once per worker
incarnation; initialization failures return an `io::Error`. The state is
cloned for connections, so your real pool type should clone a shared handle,
not open a new pool on every clone. Do not initialize a pool in a handler or
in a lazily created route service.

For a real database, make the trait return a `Result` as well as `Option`, so
missing rows remain distinct from connection and query failures. A per-worker
pool's configured capacity is multiplied by the number of workers. If your
chosen pool is thread-safe and intended to be process-wide, initialize one
outside the server and share its handle instead. See
[Application State](./8.2-web-app.md),
[Worker and Request State](./6-server-app.md), and
[Error Handling](./8.5-web-app.md).

## A periodic task per worker

Use this for worker-local cache refreshes or housekeeping. Start the task in
the worker state factory, not in a handler or per-connection application
service. This example increments a local counter once per interval and
exposes it at `/ticks`. Each worker has its own counter, so separate requests
can observe different values when they reach different workers.

```rust
use ntex::{
    SharedCfg, rt,
    time::{self, Seconds},
    web,
};
use std::{cell::Cell, rc::Rc};

#[derive(Clone)]
struct Worker {
    ticks: Rc<Cell<u64>>,
    _task: Rc<rt::JoinHandle<()>>,
}
type State = web::AppState<Worker>;

async fn ticks(state: &State, (): ()) -> String {
    state.ticks.get().to_string()
}

fn start_worker() -> State {
    let ticks = Rc::new(Cell::new(0));
    let counter = ticks.clone();
    let task = rt::spawn(async move {
        let interval = time::interval(Seconds(5));
        loop {
            interval.tick().await;
            counter.set(counter.get() + 1);
        }
    })
    .cancel_on_drop();
    web::AppState::new(Worker {
        ticks,
        _task: Rc::new(task),
    })
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::with_config(
        async || Ok(start_worker()),
        async |_| web::App::new().route("/ticks", web::get().to_with_state(ticks)),
    )
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

The first tick waits one period; `interval` does not immediately run the job.
For an initial refresh, do it explicitly before entering the loop. Await each
refresh before the next tick to avoid overlapping runs. Use a single task to
wait on each interval, and avoid blocking calls in that task.

The state keeps the task handle in an `Rc`, shared by its connection clones.
`cancel_on_drop()` requests cancellation when the last state handle is
dropped; worker runtime teardown also drops pending tasks. The task captures
only the counter, not its owning state, so it does not create an ownership
cycle. Dropping an ordinary task handle without `cancel_on_drop` would detach
the task instead.

Cancellation drops a pending future; it is not an asynchronous cleanup hook.
If a job must flush or commit work, arrange a stop notification and await its
completion before stopping the server, with an explicit deadline. Do not
rely on worker teardown to finish that work. See
[Timers](./12-time.md), [Runtime](./4-runtime.md), and
[Server Shutdown](./5-server.md#signals-and-shutdown).

## Compression and a fixed-origin CORS policy

Use this for a browser-facing endpoint that serves substantial text and is
readable by one known frontend origin. Enable ntex's `compress` feature.
`Compress` negotiates the response encoding from `Accept-Encoding`; the
second middleware adds a small, fixed CORS policy and handles preflight
requests for this read-only API.

```rust
use ntex::web::{
    HttpResponse,
    middleware::{Compress, from_fn},
};
use ntex::{
    SharedCfg,
    http::{
        Method,
        header::{HeaderName, HeaderValue},
    },
    web,
};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| {
        web::App::new()
            .middleware(Compress::default())
            .middleware(from_fn(async |req, next| {
                let mut response = if req.method() == Method::OPTIONS {
                    req.into_response(HttpResponse::NoContent().build())
                } else {
                    next.call(req).await?
                };
                for (name, value) in [
                    ("access-control-allow-origin", "https://frontend.example"),
                    ("access-control-allow-methods", "GET, OPTIONS"),
                ] {
                    response.headers_mut().insert(
                        HeaderName::from_static(name),
                        HeaderValue::from_static(value),
                    );
                }
                Ok(response)
            }))
            .route(
                "/report",
                web::get().to(async || {
                    HttpResponse::Ok()
                        .content_type("text/plain")
                        .body("report\n".repeat(500))
                }),
            )
    })
    .bind("127.0.0.1:8080", SharedCfg::default())?
    .run()
    .await
}
```

Known-size bodies below 1 KiB stay uncompressed. For negotiated responses,
compression middleware adds `Vary: Accept-Encoding` so shared caches do not
mix encodings. Streaming responses keep the encoder until the body ends;
account for that memory when many streams remain open.

The CORS origin is constant, not reflected from an arbitrary request. This
example allows no credentials and no extra request headers. A browser asking
for an unlisted method or non-safelisted header will not get permission.
If you need authenticated browser calls, a changing origin allowlist, or
route-specific preflight rules, use a complete CORS implementation instead
of growing this example into one. CORS controls browser access, not which
clients may call your server. It is not authentication. See
[Filters and Middleware](./8.3-web-app.md) for layering and
[Routing and Handlers](./8.1-web-app.md) for method-specific routes.

## Listening on more than one address

Use this when the same application should accept connections on several
interfaces or ports. Chain `bind` calls on one `HttpServer`; each listener
gets its own `SharedCfg`, so you can choose different protocol settings
without running separate applications. These two listeners deliberately
share one application definition and the same health endpoint.

```rust
use ntex::{SharedCfg, web};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::HttpServer::new(async |_| web::App::new().route("/health", web::get().to(async || "ok")))
        .bind("127.0.0.1:8080", SharedCfg::new("first-listener"))?
        .bind("127.0.0.1:8081", SharedCfg::new("second-listener"))?
        .run()
        .await
}
```

Binding errors are returned before `run()`: a port already in use should
fail startup rather than silently leave one address unavailable. A hostname
can resolve to multiple socket addresses as well. Choose explicit interface
addresses when you need predictable exposure; loopback listeners are not
reachable from other machines.

The factory is used for each listener on each worker. Do not assume mutable
state constructed inside that closure is shared between listeners. Put
worker-wide resources in `HttpServer::with_config` state, or clone a
process-wide `Arc` into every application.

For HTTP and HTTPS together, keep a plain `bind` and add a backend-specific
`bind_openssl` or `bind_rustls` with its configured TLS acceptor. Certificate
loading, backend features, and a complete compiling HTTPS example are in
[TLS](./10-tls.md); keep those checks rather than disabling verification to
get a listener running. See [Server](./5-server.md) for binding existing
listeners and controlling the resulting server.
