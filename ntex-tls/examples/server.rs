use std::io;

use ntex::service::{chain_factory, fn_service};
use ntex::{codec, io::Io, server, util::Either, ServiceFactory};
use ntex_tls::openssl::{PeerCert, PeerCertChain, SslAcceptor};
use tls_openssl::ssl::{self, SslFiletype, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    println!("Started openssl echp server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    builder.set_verify_callback(SslVerifyMode::PEER, |_success, _ctx| true);
    builder
        .set_private_key_file("./examples/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./examples/cert.pem")
        .unwrap();
    let acceptor = builder.build();

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move |_| {
            chain_factory(SslAcceptor::new(acceptor.clone())).and_then(
                fn_service(|io: Io<_>| async move {
                    println!("New client is connected");
                    if let Some(cert) = io.query::<PeerCert>().as_ref() {
                        println!("Peer cert: {:?}", cert.0);
                    }
                    if let Some(cert) = io.query::<PeerCertChain>().as_ref() {
                        println!("Peer cert chain: {:?}", cert.0);
                    }
                    loop {
                        match io.recv(&codec::BytesCodec).await {
                            Ok(Some(msg)) => {
                                println!("Got message: {:?}", msg);
                                io.send(msg.freeze(), &codec::BytesCodec)
                                    .await
                                    .map_err(Either::into_inner)?;
                            }
                            Err(e) => {
                                println!("Got error: {:?}", e);
                                break;
                            }
                            Ok(None) => break,
                        }
                    }
                    println!("Client is disconnected");
                    Ok(())
                })
                .map_init_err(|_| ()),
            )
        })?
        .workers(1)
        .run()
        .await
}
