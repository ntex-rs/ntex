# Web Applications and Routing

ntex provides the [`ntex::web`] framework for building HTTP applications. Its
application, routing, request-extraction, response, middleware, and testing
APIs are available through the `ntex::web` module.

[`web::App`] is the top-level application builder. It collects resources,
routes, scopes, middleware, filters, and fallback services, then builds the
service that handles HTTP requests.

An application factory is normally created once for each server worker:

```rust,no_run
use ntex::web::{self, HttpResponse};

#[ntex::main]
async fn main() -> std::io::Result<()> {
    web::server(async |_| {
        web::App::new()
            .route(
                "/",
                web::get().to(async || HttpResponse::Ok().body("Hello")),
            )
    })
    .bind("127.0.0.1:8080", ntex::SharedCfg::default())?
    .run()
    .await
}
```

Each worker owns its application and services. See [Server](./5-server.md) and
[Application state management](./6-server-app.md) for details about workers,
server configuration, and application state.

## Handlers

There are multiple ways to register handlers in the framework.
`App::route()` is the shortest way to register one route:

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new()
    .route("/users", web::get().to(async || HttpResponse::Ok()))
    .route(
        "/users",
        web::post().to(async || HttpResponse::Created()),
    );
```

Handler arguments are request extractors that implement [`FromRequest`], and
the return value must implement [`Responder`].

Internally, each `App::route()` call creates a [`Resource`] with one [`Route`]
and promotes the route's method and custom guards to resource guards. This lets
several calls register the same path with different guards, but it also affects
fallback behavior: if those guards fail, the resource is not selected and the
application default runs, returning `404 Not Found` by default.

Use `App::service()` and `web::resource()` when several routes, a resource
name, resource middleware or filters, or a resource-specific fallback belong
to the same path:

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new().service(
    web::resource("/users/{id}")
        .name("user")
        .route(web::get().to(async || HttpResponse::Ok()))
        .route(web::delete().to(async || HttpResponse::NoContent())),
);
```

`Resource::to()` is shorthand for adding an unrestricted route. Because it has
no method guard, it accepts every HTTP method:

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new().service(
    web::resource("/health").to(async || HttpResponse::Ok()),
);
```

ntex also provides attribute macros. The generated item implements the web
service-factory interface and can be passed directly to `App::service()`:

```rust
use ntex::web::{self, HttpResponse};

#[web::get("/health")]
async fn health() -> HttpResponse {
    HttpResponse::Ok().into()
}

let app = web::App::<()>::new().service(health);
```

Method-specific route helpers include `get`, `post`, `put`, `delete`, `patch`,
`head`, `options`, `trace`, `connect`, and `query`.

## How Routing Works

For each request, ntex performs routing in stages:

1. Application middleware wraps the application service.
2. The application filter processes the incoming [`WebRequest`].
3. The application router finds a [`Resource`] whose path and resource guards
   match.
4. Resource middleware wraps the selected resource's filter and routes.
5. The resource filter processes the request.
6. The resource checks its routes in registration order. A route matches only
   if all its method and custom guards pass.
7. The first matching route calls its handler.

Middleware can return a response without calling its inner service. The later
stages run only when each enclosing middleware continues the request.

This distinction between resources and routes determines fallback behavior:

- If no resource path-and-guard combination matches, the application default
  service runs. The built-in application default returns `404 Not Found`.
- If a resource path matches but none of its routes match, the resource
  default service runs. The built-in resource default returns
  `405 Method Not Allowed`.
- A resource default is independent of the application or enclosing scope
  default.

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new()
    .service(
        web::resource("/reports")
            .route(web::get().to(async || HttpResponse::Ok()))
            .default_service(
                web::to(async || HttpResponse::MethodNotAllowed()),
            ),
    )
    .default_service(
        web::to(async || HttpResponse::NotFound().body("Unknown path")),
    );
```

Resource guards participate in application-level selection. Route guards are
checked only after a resource has been selected. Guards can match methods,
headers, or custom predicates:

```rust
use ntex::http::Method;
use ntex::web::{self, guard, HttpResponse};

let app = web::App::<()>::new().service(
    web::resource("/events").route(
        web::route()
            .method(Method::POST)
            .guard(guard::Header("content-type", "application/json"))
            .to(async || HttpResponse::Accepted()),
    ),
);
```

This distinction is useful for method handling. `App::route("/reports",
web::get().to(handler))` returns the application fallback when the request is
not `GET`, because the method guard belongs to the generated resource. By
contrast, `web::resource("/reports").route(web::get().to(handler))` selects the
resource by path first, then returns its default `405 Method Not Allowed`
response when the method guard fails. `Scope::route()` has the same shorthand
behavior as `App::route()`.

Use `App::middleware()` to wrap the complete application service. Use
`App::filter()` when every incoming `WebRequest` must be transformed before
routing. Resources and scopes provide corresponding middleware and filter
methods for narrower parts of the application.

## Route Path Format

Route patterns consist of slash-separated static and dynamic segments.
Resource and scope paths are normalized with a leading `/` when they are
registered, but writing it explicitly makes the complete route easier to read.

### Static Paths

A static pattern matches the same path:

```text
/users
/users/profile
```

Trailing slashes are significant. `/users` and `/users/` are different
patterns. Query strings are not part of path matching.

Routing is case-sensitive by default. `App::case_insensitive_routing()` makes
static path segments ASCII case-insensitive; it does not change matching
inside dynamic segments. A scope can enable the same behavior for its nested
router.

### Dynamic Segments

A dynamic segment is written as `{name}` and matches one non-empty path
segment:

```text
/users/{id}
/teams/{team}/users/{user}
```

Variables can be combined with static text, and a segment can contain more
than one variable:

```text
/releases/v{major}.{minor}
/files/{name}.{extension}
```

Use `{name:regex}` to restrict a variable with a regular expression:

```text
/users/{id:[0-9]+}
/releases/{version:v[0-9]+\.[0-9]+}
```

The expression is matched against the complete segment. Invalid patterns or
invalid regular expressions panic while the application is being built, so
route definitions should be treated as application configuration rather than
untrusted runtime input.

### Remainder Matches

Place `*` after a dynamic variable to capture the remaining path, including
embedded `/` separators:

```text
/files/{path}*
```

This pattern matches paths such as `/files/readme.txt` and
`/files/images/logo.svg`; `path` contains `readme.txt` or
`images/logo.svg`. The default remainder expression is `.*`, so the capture
may be empty. A custom regular expression cannot be combined with a remainder
match.

Static tail patterns such as `/files/*` are also supported, but a named
remainder is usually clearer and works naturally with typed path extraction.

### Multiple Patterns

A resource or scope can accept several patterns by using an array or vector:

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new()
    .service(
        web::resource(["/health", "/status"])
            .to(async || HttpResponse::Ok()),
    );
```

All patterns register the same service. A resource definition supports at
most 16 dynamic segments.

## Accessing Path Variables

Matched variables are percent-decoded and stored in the request's match
information. The usual way to access them is the typed [`Path`] extractor:

```rust
use ntex::web::{self, HttpResponse};

async fn user(path: web::types::Path<(u32,)>) -> HttpResponse {
    let user_id = path.0;
    HttpResponse::Ok().body(format!("user {user_id}"))
}

let app = web::App::<()>::new().route(
    "/users/{id:[0-9]+}",
    web::get().to(user),
);
```

Tuples deserialize variables in path order. Structs can deserialize them by
name when the struct derives `serde::Deserialize`:

```rust
use ntex::web::{self, HttpResponse};

#[derive(serde::Deserialize)]
struct UserPath {
    organization: String,
    user_id: u32,
}

async fn user(path: web::types::Path<UserPath>) -> HttpResponse {
    HttpResponse::Ok().body(format!(
        "organization: {}, user: {}",
        path.organization,
        path.user_id,
    ))
}

let app = web::App::<()>::new().route(
    "/organizations/{organization}/users/{user_id:[0-9]+}",
    web::get().to(user),
);
```

The field names must match the variable names in the route pattern.
This example requires `serde` with its `derive` feature enabled.

Handlers can also inspect [`HttpRequest::match_info()`] directly:

```rust
use ntex::web::{self, HttpRequest};

async fn file(req: HttpRequest) -> String {
    req.match_info()
        .get("path")
        .unwrap_or_default()
        .to_owned()
}

let app = web::App::<()>::new().route(
    "/files/{path}*",
    web::get().to(file),
);
```

Path segments are split before percent-decoding. An encoded slash such as
`%2F` can therefore be part of a normal dynamic variable after decoding.

## Scopes

A [`Scope`] groups services below a common path prefix:

```rust
use ntex::web::{self, HttpResponse};

let app = web::App::<()>::new().service(
    web::scope("/api")
        .service(
            web::resource("/users")
                .route(web::get().to(async || HttpResponse::Ok())),
        )
        .service(
            web::resource("/users/{id}")
                .route(web::get().to(async || HttpResponse::Ok())),
        ),
);
```

These resources match `/api/users` and `/api/users/{id}`. Scopes can be
nested, and their prefixes can contain variables. Scope variables remain
available to nested handlers:

```rust
# use ntex::web::{self, HttpResponse};
let app = web::App::<()>::new().service(
    web::scope("/organizations/{org}")
        .route(
            "/users/{user}",
            web::get().to(
                async |path: web::types::Path<(String, String)>| {
                    HttpResponse::Ok()
                        .body(format!("{}:{}", path.0, path.1))
                },
            ),
        ),
);
```

A scope prefix is a complete path-segment prefix, not a textual prefix.
`scope("/api")` handles paths below `/api/`, but does not match the bare
`/api` path. Register an explicit resource for `/api` if it needs a handler,
or use an empty nested resource pattern where appropriate.

Scopes can define their own guards, middleware, filters, case-sensitivity, and
default service. Once a scope prefix matches, unmatched nested paths use that
scope's default. If no scope default is configured, the built-in scope default
returns `404 Not Found`; a custom application default is not used.

## Named and External Resources

With ntex's `url` Cargo feature enabled, handlers can generate absolute URLs
for named resources.

Name a resource when handlers need to generate URLs for it:

```rust
use ntex::web::{self, HttpRequest, HttpResponse};

async fn index(req: HttpRequest) -> HttpResponse {
    let url = req.url_for("user", ["42"]).unwrap();
    HttpResponse::Ok().body(url.to_string())
}

let app = web::App::<()>::new()
    .service(
        web::resource("/users/{id}")
            .name("user")
            .route(web::get().to(async || HttpResponse::Ok())),
    )
    .route("/", web::get().to(index));
```

The `index` handler generates an absolute URL such as
`http://some-host-name/users/42`.

[`HttpRequest::url_for()`] substitutes supplied values in pattern order and
uses the request's connection information to produce an absolute URL.

`App::external_resource()` registers a named URL pattern for generation only:

```rust
use ntex::web;

let app = web::App::<()>::new().external_resource(
    "documentation",
    "https://docs.example.com/{page}",
);
```

The registered name can then be used for URL generation:

```rust
use ntex::web::{HttpRequest, HttpResponse};

async fn doc(req: HttpRequest) -> HttpResponse {
    let url = req.url_for("documentation", ["page1.html"]).unwrap();
    HttpResponse::Ok().body(url.to_string())
}
```

The `doc` handler generates
`https://docs.example.com/page1.html`.
External resources do not participate in request matching.

## Modular Configuration

`App::configure()` exposes a [`ServiceConfig`] so route registration can be
moved into a separate module:

```rust
use ntex::web::{self, HttpResponse};

fn configure_api(cfg: &mut web::ServiceConfig<()>) {
    cfg.route(
        "/health",
        web::get().to(async || HttpResponse::Ok()),
    );
    cfg.service(
        web::resource("/version")
            .to(async || HttpResponse::Ok().body("4")),
    );
}

let app = web::App::<()>::new()
    .configure(configure_api)
    .route("/", web::get().to(async || HttpResponse::Ok()));
```

`ServiceConfig` supports routes, services, and external resources. It does not
create a separate routing boundary; the registered items become part of the
application at the point where `configure()` is called.

Modular configuration is available at both the `App` and `Scope` levels.

## Testing Routes

The `web::test` module can initialize an `App`, construct requests, and call
the resulting service without opening a socket:

```rust
use ntex::http::StatusCode;
use ntex::web::{self, HttpResponse};
use ntex::web::test::{TestRequest, call_service, init_service};

#[ntex::test]
async fn user_route() {
    let service = init_service(
        web::App::new().route(
            "/users/{id}",
            web::get().to(async || HttpResponse::Ok()),
        ),
    )
    .await;

    let request = TestRequest::get()
        .uri("/users/42")
        .to_request();
    let response = call_service(&service, request).await;

    assert_eq!(response.status(), StatusCode::OK);
}
```

Tests should cover method mismatches, trailing slashes, dynamic-variable
constraints, scope boundaries, and fallback services because each affects a
different stage of route selection.

## Lower-Level HTTP Service

`web::server(factory)` is a convenience constructor equivalent to
`web::HttpServer::new(factory)`.

After routes and services have been registered, the resulting application
builder can be converted into a service factory from `http::Request` to
`http::Response`. [`HttpService`] uses that factory to handle HTTP/1.1 and
HTTP/2 connections.

Using the lower-level server builder also allows connection-level services to
be composed before `HttpService`. Such services can inspect or transform the
`Io` object and map the state passed into the HTTP and application services.

The equivalent lower-level server setup looks like this:

```rust,no_run
use ntex::{http, server, web, SharedCfg};
use ntex::web::HttpResponse;

#[ntex::main]
async fn main() -> std::io::Result<()> {
    server::build()
        .bind(
            "http",
            "127.0.0.1:8080",
            SharedCfg::new("S"),
            async |_| {
                http::HttpService::new(
                web::App::new()
                    .route("/", web::get().to(async || {
                        HttpResponse::Ok().body("Hello")
                    }))
                    .build(),
                )
            },
        )?
        .run()
        .await
}
```

To give the web application a fixed state that differs from the surrounding
service pipeline's state, see
[Web Application State](./9-web-app-state.md).

[`HttpRequest::match_info()`]: https://docs.rs/ntex/latest/ntex/web/struct.HttpRequest.html#method.match_info
[`HttpRequest::url_for()`]: https://docs.rs/ntex/latest/ntex/web/struct.HttpRequest.html#method.url_for
[`FromRequest`]: https://docs.rs/ntex/latest/ntex/web/trait.FromRequest.html
[`HttpService`]: https://docs.rs/ntex/latest/ntex/http/struct.HttpService.html
[`Path`]: https://docs.rs/ntex/latest/ntex/web/types/struct.Path.html
[`Resource`]: https://docs.rs/ntex/latest/ntex/web/struct.Resource.html
[`Responder`]: https://docs.rs/ntex/latest/ntex/web/trait.Responder.html
[`Route`]: https://docs.rs/ntex/latest/ntex/web/struct.Route.html
[`Scope`]: https://docs.rs/ntex/latest/ntex/web/struct.Scope.html
[`ServiceConfig`]: https://docs.rs/ntex/latest/ntex/web/struct.ServiceConfig.html
[`WebRequest`]: https://docs.rs/ntex/latest/ntex/web/struct.WebRequest.html
[`ntex::web`]: https://docs.rs/ntex/latest/ntex/web/index.html
[`web::App`]: https://docs.rs/ntex/latest/ntex/web/struct.App.html
