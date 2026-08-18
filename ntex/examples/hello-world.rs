use std::io;

use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, HttpServiceConfig, Response};
use ntex::{SharedCfg, time::Seconds};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    let cfg = SharedCfg::new("HELLO-WORLD").add(HttpServiceConfig::new().set_headers_read_rate(
        Seconds(1),
        Seconds(5),
        128,
    ));

    ntex::server::build()
        .bind("srv", "127.0.0.1:8080", cfg, async |_| {
            HttpService::new(async |_req| {
                info!("{:?}", _req);
                let mut res = Response::Ok();
                res.header("x-head", HeaderValue::from_static("dummy value!"));
                Ok::<_, io::Error>(res.body("Hello world!"))
            })
        })?
        .run()
        .await
}
