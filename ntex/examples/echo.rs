use std::{env, io};

use futures::StreamExt;
use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, Request, Response};
use ntex::server::Server;
use ntex::util::BytesMut;

#[ntex::main]
async fn main() -> io::Result<()> {
    env::set_var("RUST_LOG", "echo=info");
    env_logger::init();

    Server::build()
        .bind("echo", "127.0.0.1:8080", || {
            HttpService::build()
                .client_timeout(1000)
                .disconnect_timeout(1000)
                .finish(|mut req: Request| async move {
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
                .tcp()
        })?
        .run()
        .await
}
