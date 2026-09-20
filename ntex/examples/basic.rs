//! Web application example with routes, extractors, and middleware.
//!
//! Run it and try:
//! `curl http://127.0.0.1:8081/resource1/Ada/index.html`

use log::info;
use ntex::web::{self, App, HttpRequest, HttpResponse, HttpServer, middleware};
use ntex::{SharedCfg, http};

#[web::get("/resource1/{name}/index.html")]
async fn greet(req: HttpRequest, name: web::types::Path<String>) -> String {
    info!("{} {}", req.method(), req.path());
    format!("Hello, {name}!\n")
}

async fn resource_index(req: HttpRequest) -> &'static str {
    info!("{} {}", req.method(), req.path());
    "Resource 2\n"
}

#[web::get("/")]
async fn home() -> &'static str {
    "Hello from ntex!\n"
}

#[ntex::main(name = "basic", signals = true)]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    info!("starting web server at http://127.0.0.1:8081");

    HttpServer::new(async |_| {
        App::new()
            .middleware(middleware::Logger::default())
            .service((greet, home))
            .service(
                web::resource("/resource2/index.html")
                    .middleware(ntex::util::timeout::Timeout::new(ntex::time::Millis(5000)))
                    .middleware(
                        middleware::DefaultHeaders::new().header("x-example-version", "0.3"),
                    )
                    .default_service(web::route().to(async || HttpResponse::MethodNotAllowed()))
                    .route(web::get().to(resource_index)),
            )
            .service(web::resource("/health").to(async || "healthy\n"))
    })
    .bind(
        "0.0.0.0:8081",
        SharedCfg::new("MY-SERVER")
            .add(http::HttpServiceConfig::new().set_keepalive(http::KeepAlive::Disabled)),
    )?
    .workers(1)
    .run()
    .await
}
