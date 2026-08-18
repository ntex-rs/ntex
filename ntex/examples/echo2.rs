use std::io;

use futures_util::StreamExt;
use ntex::http::{HttpService, Request, Response, header::HeaderValue};
use ntex::{SharedCfg, util::BytesMut};

async fn handle_request(mut req: Request) -> Result<Response, io::Error> {
    let mut body = BytesMut::new();
    while let Some(item) = req.payload().next().await {
        body.extend_from_slice(&item.unwrap())
    }

    log::info!("request body: {:?}", body);
    Ok(Response::Ok()
        .header("x-head", HeaderValue::from_static("dummy value!"))
        .body(body))
}

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    ntex::server::build()
        .bind("echo", "127.0.0.1:8080", SharedCfg::default(), async || {
            HttpService::h2(handle_request)
        })?
        .run()
        .await
}
