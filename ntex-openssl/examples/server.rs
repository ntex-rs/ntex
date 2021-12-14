use std::io;

use ntex::service::{fn_service, pipeline_factory};
use ntex::{codec, io::filter_factory, io::into_io, io::Io, server, util::Either};
use ntex_openssl::SslAcceptor;
use openssl::ssl::{self, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Started openssl echp server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("../tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("../tests/cert.pem")
        .unwrap();
    let acceptor = builder.build();

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move || {
            pipeline_factory(into_io())
                .and_then(filter_factory(SslAcceptor::new(acceptor.clone())))
                .and_then(fn_service(|io: Io<_>| async move {
                    println!("New client is connected");
                    loop {
                        match io.next(&codec::BytesCodec).await {
                            Ok(Some(msg)) => {
                                println!("Got message: {:?}", msg);
                                io.send(msg.freeze(), &codec::BytesCodec)
                                    .await
                                    .map_err(Either::into_inner)?;
                            }
                            Ok(None) => break,
                            Err(e) => {
                                println!("Got error: {:?}", e);
                                break;
                            }
                        }
                    }
                    println!("Client is disconnected");
                    Ok(())
                }))
        })?
        .workers(1)
        .run()
        .await
}
