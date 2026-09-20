//! Minimal server built directly with `HttpService`.
//!
//! Run it and try: `curl -i http://127.0.0.1:8080/`

use std::io;

use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, HttpServiceConfig, Request, Response};
use ntex::{SharedCfg, time::Seconds};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    let cfg = SharedCfg::new("HELLO-WORLD").add(HttpServiceConfig::new().set_headers_read_rate(
        Seconds(1),
        Seconds(5),
        128,
    ));

    info!("starting HTTP server at http://127.0.0.1:8080");
    ntex::server::build()
        .bind("hello-world", "127.0.0.1:8080", cfg, async |_| {
            HttpService::new(async |req: Request| {
                info!("{} {}", req.method(), req.path());
                let mut res = Response::Ok();
                res.header("x-example", HeaderValue::from_static("hello-world"));
                Ok::<_, io::Error>(res.body("Hello from ntex!\n"))
            })
        })?
        .run()
        .await
}
