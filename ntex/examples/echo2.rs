//! HTTP/2-only echo server using the low-level `HttpService` API.
//!
//! Connect with an HTTP/2-capable client and send a request body to
//! `http://127.0.0.1:8080/echo`.

use std::io;

use futures_util::StreamExt;
use ntex::http::{HttpService, Request, Response, header};
use ntex::{SharedCfg, util::BytesMut};

async fn handle_request(mut req: Request) -> Result<Response, io::Error> {
    let mut body = BytesMut::new();
    while let Some(chunk) = req.payload().next().await {
        let chunk = chunk.map_err(|err| io::Error::other(err.to_string()))?;
        body.extend_from_slice(&chunk);
    }

    log::info!(
        "{} {}: echoing {} bytes over HTTP/2",
        req.method(),
        req.path(),
        body.len()
    );
    Ok(Response::Ok()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(body))
}

#[ntex::main]
async fn main() -> io::Result<()> {
    env_logger::init();
    log::info!("starting HTTP/2 echo server at http://127.0.0.1:8080");

    ntex::server::build()
        .bind(
            "h2-echo",
            "127.0.0.1:8080",
            SharedCfg::default(),
            async |_| HttpService::h2(handle_request),
        )?
        .run()
        .await
}
