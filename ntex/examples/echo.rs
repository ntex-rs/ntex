use std::io;

use futures_util::StreamExt;
use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, HttpServiceConfig, Request, Response};
use ntex::{SharedCfg, time::Seconds, util::BytesMut};

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    ntex::server::build()
        .bind("echo", "127.0.0.1:8080", async |_| {
            HttpService::new(|mut req: Request| async move {
                let mut body = BytesMut::new();
                while let Some(item) = req.payload().next().await {
                    body.extend_from_slice(&item.unwrap());
                }

                info!("request body: {:?}", body);
                Ok::<_, io::Error>(
                    Response::Ok()
                        .header("x-head", HeaderValue::from_static("dummy value!"))
                        .body(body),
                )
            })
        })?
        .config(
            "echo",
            SharedCfg::new("ECHO").add(HttpServiceConfig::new().set_headers_read_rate(
                Seconds(1),
                Seconds(5),
                128,
            )),
        )
        .run()
        .await
}
