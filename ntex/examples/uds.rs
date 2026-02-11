use ntex::web::{self, HttpRequest};

#[web::get("/resource1/{name}/index.html")]
async fn index(req: HttpRequest, name: web::types::Path<String>) -> String {
    println!("REQ: {:?}", req);
    format!("Hello: {}!\r\n", name)
}

#[cfg(unix)]
async fn index_async(req: HttpRequest) -> Result<&'static str, std::io::Error> {
    println!("REQ: {:?}", req);
    Ok("Hello world!\r\n")
}

#[web::get("/")]
async fn no_params() -> &'static str {
    "Hello world!\r\n"
}

#[cfg(unix)]
#[ntex::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    use ntex::web::{App, HttpResponse, middleware};

    web::HttpServer::new(async || {
        App::new()
            .middleware(middleware::DefaultHeaders::new().header("X-Version", "0.2"))
            .middleware(middleware::Logger::default())
            .service((index, no_params))
            .service(
                web::resource("/resource2/index.html")
                    .middleware(
                        middleware::DefaultHeaders::new().header("X-Version-R2", "0.3"),
                    )
                    .default_service(
                        web::route().to(|| async { HttpResponse::MethodNotAllowed() }),
                    )
                    .route(web::get().to(index_async)),
            )
            .service(web::resource("/test1.html").to(|| async { "Test\r\n" }))
    })
    .bind_uds("/tmp/uds-test")?
    .workers(1)
    .run()
    .await
}

#[cfg(not(unix))]
fn main() {}
