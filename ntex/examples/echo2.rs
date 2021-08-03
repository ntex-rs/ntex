use std::{env, io};

use futures::StreamExt;
use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, Request, Response};
use ntex::server::Server;
use ntex::util::BytesMut;

async fn handle_request(mut req: Request) -> Result<Response, io::Error> {
    let mut body = BytesMut::new();
    while let Some(item) = req.payload().next().await {
        body.extend_from_slice(&item.unwrap())
    }

    info!("request body: {:?}", body);
    Ok(Response::Ok()
        .header("x-head", HeaderValue::from_static("dummy value!"))
        .body(body))
}

#[ntex::main]
async fn main() -> io::Result<()> {
    env::set_var("RUST_LOG", "echo=info");
    env_logger::init();

    Server::build()
        .bind("echo", "127.0.0.1:8080", || {
            HttpService::build().finish(handle_request).tcp()
        })?
        .run()
        .await
}
