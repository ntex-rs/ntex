use std::io;

use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, HttpServiceConfig, Response};
use ntex::{io::SharedConfig, time::Seconds, util::Ready};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    ntex::server::build()
        .bind("hello-world", "127.0.0.1:8080", |_| {
            HttpService::new(|_req| {
                info!("{:?}", _req);
                let mut res = Response::Ok();
                res.header("x-head", HeaderValue::from_static("dummy value!"));
                Ready::Ok::<_, io::Error>(res.body("Hello world!"))
            })
        })?
        .config(
            "hello-world",
            SharedConfig::build("HELLO-WORLD").add(
                HttpServiceConfig::new().headers_read_rate(Seconds(1), Seconds(5), 128),
            ),
        )
        .run()
        .await
}
