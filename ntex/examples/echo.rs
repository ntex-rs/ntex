//! HTTP echo server that buffers and returns each request body.
//!
//! Run it and try: `curl --data 'hello' http://127.0.0.1:8080/echo`

use std::io;

use futures_util::StreamExt;
use log::info;
use ntex::http::{HttpService, HttpServiceConfig, Request, Response, header};
use ntex::{SharedCfg, time::Seconds, util::BytesMut};

async fn echo(mut req: Request) -> Result<Response, io::Error> {
    let mut body = BytesMut::new();
    while let Some(chunk) = req.payload().next().await {
        let chunk = chunk.map_err(|err| io::Error::other(err.to_string()))?;
        body.extend_from_slice(&chunk);
    }

    info!(
        "{} {}: echoing {} bytes",
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

    let cfg = SharedCfg::new("ECHO").add(HttpServiceConfig::new().set_headers_read_rate(
        Seconds(1),
        Seconds(5),
        128,
    ));

    info!("starting HTTP echo server at http://127.0.0.1:8080");
    ntex::server::build()
        .bind("echo", "127.0.0.1:8080", cfg, async |_| {
            HttpService::new(echo)
        })?
        .run()
        .await
}
