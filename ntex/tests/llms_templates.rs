//! Every ```rust template in `docs/llms.txt`, compiled verbatim.
//!
//! Item templates live in their own module; statement fragments are wrapped in a
//! function. `templates_match_llms_txt` fails when a template changes on either side.
#![allow(dead_code, unreachable_pub, unused_must_use)]

#[rustfmt::skip]
mod function_service {
    pub async fn template() -> Result<(), Box<dyn std::error::Error>> {
        use ntex::service::{Pipeline, fn_service};

        let service = fn_service(async |req: u32| Ok::<_, std::io::Error>(req + 1));
        let pipeline = Pipeline::new((), service);
        assert_eq!(pipeline.call(1).await?, 2);
        Ok(())
    }
}

#[rustfmt::skip]
mod minimal_web_server {
    use ntex::{SharedCfg, web};

    #[web::get("/")]
    async fn index() -> &'static str {
        "Hello world!"
    }

    #[ntex::main]
    async fn main() -> std::io::Result<()> {
        web::HttpServer::new(async |_| web::App::new().service(index))
            .bind("127.0.0.1:8080", SharedCfg::new("hello-world"))?
            .run()
            .await
    }
}

#[rustfmt::skip]
mod closure_web_middleware {
    fn template() {
        use ntex::http::header::{HeaderName, HeaderValue};
        use ntex::web::{self, App, HttpResponse, middleware::from_fn};

        web::HttpServer::new(async |_| {
            App::new()
                .middleware(from_fn(async |req, next| {
                    if !req.headers().contains_key("authorization") {
                        return Ok(req.into_response(HttpResponse::Unauthorized().build()));
                    }
                    let mut res = next.call(req).await?;
                    res.headers_mut().insert(
                        HeaderName::from_static("x-application"),
                        HeaderValue::from_static("example"),
                    );
                    Ok(res)
                }))
                .route("/", web::get().to(async || "Hello"))
        });
    }
}

#[rustfmt::skip]
mod struct_middleware {
    use std::time::Instant;

    use ntex::web::{WebRequest, WebResponse};
    use ntex::{Ctx, Middleware, Service};

    pub struct Timing;

    pub struct TimingService<S> {
        service: S,
    }

    impl<S, St> Middleware<S, St> for Timing {
        type Service = TimingService<S>;

        fn create(&self, _: &St, service: S) -> Self::Service {
            TimingService { service }
        }
    }

    impl<S, St, ReqSt> Service<St, WebRequest<ReqSt>> for TimingService<S>
    where
        S: Service<St, WebRequest<ReqSt>, Res = WebResponse>,
    {
        type Res = WebResponse;
        type Error = S::Error;

        ntex::forward!(St, service);

        async fn call(
            &self,
            req: WebRequest<ReqSt>,
            ctx: Ctx<'_, Self, St>,
        ) -> Result<WebResponse, S::Error> {
            let start = Instant::now();
            let res = ctx.call(&self.service, req).await;
            log::info!("request took {:?}", start.elapsed());
            res
        }
    }
}

#[rustfmt::skip]
mod forward_map_err {
    use ntex::{Ctx, Service};

    pub enum AuthError<E> {
        Denied,
        Service(E),
    }

    pub struct Authorized<A, S> {
        check: A,
        service: S,
    }

    impl<A, S, St, Req> Service<St, Req> for Authorized<A, S>
    where
        A: Service<St, Req, Res = (Req, bool), Error = S::Error>,
        S: Service<St, Req>,
    {
        type Res = S::Res;
        type Error = AuthError<S::Error>;

        ntex::forward!(St, check, service, map_err = AuthError::Service);

        async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, Self::Error> {
            let (req, allowed) = ctx.call(&self.check, req).await.map_err(AuthError::Service)?;
            if !allowed {
                return Err(AuthError::Denied);
            }
            ctx.call(&self.service, req).await.map_err(AuthError::Service)
        }
    }
}

#[rustfmt::skip]
mod shared_pipeline {
    use ntex::service::{BindCtx, Ctx, Pipeline, Service};

    pub struct Cached<S, E> {
        service: S,
        // shared with other services, owned and shut down elsewhere
        cache: Pipeline<String, Option<String>, E>,
    }

    impl<S, St, E> Service<St, String> for Cached<S, E>
    where
        S: Service<St, String, Res = String, Error = E>,
        E: 'static,
    {
        type Res = String;
        type Error = E;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.bind(&self.service);
            self.cache.bind_to(&ctx);
        }

        async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), E> {
            self.cache.ready().await?;
            ctx.ready(&self.service).await
        }

        async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
            // the shared cache pipeline is shut down by its owner
            ctx.shutdown(&self.service).await;
        }

        async fn call(&self, req: String, ctx: Ctx<'_, Self, St>) -> Result<String, E> {
            if let Some(res) = self.cache.call(req.clone()).await? {
                return Ok(res);
            }
            ctx.call(&self.service, req).await
        }
    }
}

#[rustfmt::skip]
mod tcp_server {
    use ntex::{SharedCfg, codec::BytesCodec, io::Io, service::fn_service};

    async fn echo(io: Io) -> std::io::Result<()> {
        while let Some(msg) = io.recv(&BytesCodec).await? {
            io.send(msg, &BytesCodec).await?;
        }
        Ok(())
    }

    #[ntex::main]
    async fn main() -> std::io::Result<()> {
        ntex::server::build()
            .bind("echo", "127.0.0.1:8080", SharedCfg::default(), async |_| fn_service(echo))?
            .run()
            .await
    }

    /// STATEMENT: The TCP server template echoes received bytes back to a client.
    ///
    /// HOW:
    /// - Start the documented `echo` service with `test_server`.
    /// - Connect, send one byte with `BytesCodec`, and assert the same byte is returned.
    #[ntex::test]
    async fn echo_server() -> std::io::Result<()> {
        use ntex::util::Bytes;

        let srv = ntex::server::test_server(async |_| fn_service(echo));
        let io = srv.connect().await?;
        io.send(Bytes::from_static(b"x"), &BytesCodec).await?;
        assert_eq!(io.recv(&BytesCodec).await?.unwrap(), "x");
        Ok(())
    }
}

#[rustfmt::skip]
mod dispatcher_server {
    use ntex::codec::BytesCodec;
    use ntex::io::{DispatchItem, Dispatcher, Io};
    use ntex::service::{Pipeline, fn_service};

    async fn serve(io: Io) -> std::io::Result<()> {
        let handler = fn_service(async |item: DispatchItem<BytesCodec>| match item {
            DispatchItem::Item(frame) => Ok::<_, std::io::Error>(Some(frame)),
            _ => Ok(None),
        });
        Dispatcher::new(io, BytesCodec, Pipeline::new((), handler)).await
    }

    #[ntex::main]
    async fn main() -> std::io::Result<()> {
        ntex::server::build()
            .bind("echo", "127.0.0.1:8080", ntex::SharedCfg::default(), async |_| fn_service(serve))?
            .run()
            .await
    }

    /// STATEMENT: The dispatcher template answers each decoded frame with the
    /// handler's response.
    ///
    /// HOW:
    /// - Start `serve` with `test_server` and connect a client `Io`.
    /// - Send one byte with `BytesCodec` and assert the response contains the same byte.
    #[ntex::test]
    async fn dispatcher_echo() -> std::io::Result<()> {
        let srv = ntex::server::test_server(async |_| fn_service(serve));
        let io = srv.connect().await?;
        io.send(ntex::util::Bytes::from_static(b"x"), &BytesCodec).await?;
        assert_eq!(io.recv(&BytesCodec).await?.unwrap(), "x");
        Ok(())
    }
}

#[rustfmt::skip]
mod http_service {
    use ntex::SharedCfg;
    use ntex::http::{HttpService, Request, Response};

    async fn handle(req: Request) -> Result<Response, std::io::Error> {
        Ok(Response::Ok().body(format!("path: {}", req.path())))
    }

    #[ntex::main]
    async fn main() -> std::io::Result<()> {
        ntex::server::build()
            .bind("http", "127.0.0.1:8080", SharedCfg::default(), async |_| HttpService::new(handle))?
            .run()
            .await
    }

    /// STATEMENT: The low-level HTTP template serves the requested path to an HTTP client.
    ///
    /// HOW:
    /// - Start the documented `handle` service with `http::test::server`.
    /// - Send `GET /test` and use `?` on the client send and body futures.
    /// - Assert a success status and the body `path: /test`.
    #[ntex::test]
    async fn http_server() -> Result<(), Box<dyn std::error::Error>> {
        use ntex::http::{Method, test};

        let srv = test::server(async |_| HttpService::new(handle));
        let res = srv.request(Method::GET, "/test").send().await?;
        assert!(res.status().is_success());
        assert_eq!(res.body().await?, "path: /test");
        Ok(())
    }
}

#[rustfmt::skip]
mod http_client {
    use ntex::client::Client;
    use ntex::http::{HttpService, Response};

    /// STATEMENT: A test server serves an HTTP response to `Client`.
    ///
    /// HOW:
    /// - Start a server on an available port with an async factory accepting unit state.
    /// - Send `GET /`, then check the status and body using `?` for client errors.
    #[ntex::test]
    async fn http_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
        let srv = ntex::server::test_server(async |_| {
            HttpService::new(async |_| Ok::<_, std::io::Error>(Response::Ok().body("hello")))
        });
        let url = format!("http://{}/", srv.addr());
        let res = Client::new().get(url.as_str()).send().await?;
        assert!(res.status().is_success());
        assert_eq!(res.body().await?, "hello");
        Ok(())
    }
}

#[rustfmt::skip]
mod worker_state {
    use std::{cell::Cell, rc::Rc};

    use ntex::{SharedCfg, io::Io};

    #[derive(Clone, Default)]
    struct Connections(Rc<Cell<usize>>);

    async fn handle(st: &Connections, _io: Io) -> std::io::Result<()> {
        st.0.set(st.0.get() + 1);
        Ok(())
    }

    #[ntex::main]
    async fn main() -> std::io::Result<()> {
        ntex::server::build_with_config(async || Ok(Connections::default()))
            .bind("svc", "127.0.0.1:8080", SharedCfg::default(), async |_| ntex::service(handle))?
            .run()
            .await
    }
}

fn normalize(text: &str) -> String {
    text.lines().map(str::trim).collect::<Vec<_>>().join("\n")
}

/// STATEMENT: Every ```rust block in `docs/llms.txt` appears verbatim in this file,
/// so documented templates are always compiled.
///
/// HOW:
/// - Extract each ```rust block from `docs/llms.txt`.
/// - Strip indentation from both the block and this file.
/// - Assert at least one block exists and each block is contained in this file.
#[test]
fn templates_match_llms_txt() {
    // checkouts may use CRLF line endings (e.g. git `core.autocrlf` on Windows)
    let docs = include_str!("../../docs/llms.txt").replace("\r\n", "\n");
    let source = normalize(include_str!("llms_templates.rs"));

    let mut blocks = 0;
    let mut rest = docs.as_str();
    while let Some(start) = rest.find("```rust\n") {
        let body = &rest[start + "```rust\n".len()..];
        let end = body.find("\n```").expect("unterminated ```rust block");
        let block = normalize(&body[..end]);
        assert!(
            source.contains(&block),
            "docs/llms.txt template is not compiled by tests/llms_templates.rs:\n{block}"
        );
        blocks += 1;
        rest = &body[end..];
    }
    assert!(blocks > 0, "no ```rust blocks found in docs/llms.txt");
}

/// STATEMENT: The function-service template runs and produces the documented result.
///
/// HOW:
/// - Run the wrapped template, whose `assert_eq!` checks that the pipeline returns `2`.
/// - Assert the wrapper returns `Ok`.
#[ntex::test]
async fn function_service_template_runs() {
    function_service::template().await.unwrap();
}
