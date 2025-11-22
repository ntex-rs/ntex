use std::io;

use log::info;
use ntex::http::header::HeaderValue;
use ntex::http::{HttpService, Response};
use ntex::{server, time::Seconds, util::Ready};
use tls_openssl::ssl::{self, SslFiletype, SslMethod};

#[ntex::main]
async fn main() -> io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    let _ = env_logger::try_init();

    println!("Started openssl web server: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./examples/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./examples/cert.pem")
        .unwrap();

    // h2 alpn config
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else {
            Err(ssl::AlpnError::NOACK)
        }
    });
    builder.set_alpn_protos(b"\x02h2").unwrap();

    let acceptor = builder.build();

    // start server
    server::ServerBuilder::new()
        .bind("basic", "127.0.0.1:8443", move |_| {
            HttpService::build()
                .client_timeout(Seconds(1))
                .h2(|req| {
                    info!("{:?}", req);
                    let mut res = Response::Ok();
                    res.header("x-head", HeaderValue::from_static("dummy value!"));
                    Ready::Ok::<_, io::Error>(res.body("Hello world!"))
                })
                .openssl(acceptor.clone())
        })?
        .workers(1)
        .run()
        .await
}
