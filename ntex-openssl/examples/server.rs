use std::io;

use ntex::service::{fn_service, pipeline_factory};
use ntex::{io::filter_factory, io::into_io, server};
use ntex_openssl::SslAcceptor;
use openssl::ssl::{self, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Started http server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./examples/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./examples/cert.pem")
        .unwrap();
    let acceptor = builder.build();

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move || {
            pipeline_factory(into_io())
                .and_then(filter_factory(SslAcceptor::new(acceptor.clone())))
                .and_then(fn_service(|io| async move {
                    println!("DONE");
                    Ok(())
                }))
        })?
        .workers(1)
        .run()
        .await
}
