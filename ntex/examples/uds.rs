//! Web server listening on a Unix domain socket.
//!
//! Run it and try:
//! `curl --unix-socket /tmp/ntex-example.sock http://localhost/`

use log::info;
use ntex::web::{self, HttpRequest};

#[web::get("/resource1/{name}/index.html")]
async fn greet(req: HttpRequest, name: web::types::Path<String>) -> String {
    info!("{} {}", req.method(), req.path());
    format!("Hello, {name}!\n")
}

#[cfg(unix)]
async fn resource_index(req: HttpRequest) -> Result<&'static str, std::io::Error> {
    info!("{} {}", req.method(), req.path());
    Ok("Resource 2\n")
}

#[web::get("/")]
async fn home() -> &'static str {
    "Hello from ntex over a Unix socket!\n"
}

#[cfg(unix)]
#[ntex::main]
async fn main() -> std::io::Result<()> {
    const SOCKET_PATH: &str = "/tmp/ntex-example.sock";

    use ntex::SharedCfg;
    use ntex::web::{App, HttpResponse, middleware};

    env_logger::init();
    if std::path::Path::new(SOCKET_PATH).exists() {
        std::fs::remove_file(SOCKET_PATH)?;
    }
    info!("starting Unix socket server at {SOCKET_PATH}");

    web::HttpServer::new(async |_| {
        App::new()
            .middleware(middleware::DefaultHeaders::new().header("x-example-version", "0.2"))
            .middleware(middleware::Logger::default())
            .service((greet, home))
            .service(
                web::resource("/resource2/index.html")
                    .middleware(
                        middleware::DefaultHeaders::new().header("x-example-version", "0.3"),
                    )
                    .default_service(web::route().to(async || HttpResponse::MethodNotAllowed()))
                    .route(web::get().to(resource_index)),
            )
            .service(web::resource("/health").to(async || "healthy\n"))
    })
    .bind_uds(SOCKET_PATH, SharedCfg::default())?
    .workers(1)
    .run()
    .await
}

#[cfg(not(unix))]
fn main() {
    eprintln!("this example requires Unix domain socket support");
}
