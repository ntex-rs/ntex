use ntex::SharedCfg;
use ntex::client::{Client, Connector, error::SendRequestError};
use tls_openssl::ssl::{self, SslMethod, SslVerifyMode};

#[ntex::main]
async fn main() -> Result<(), SendRequestError> {
    env_logger::init();
    println!("Connecting to openssl webserver: 127.0.0.1:8443");

    // load ssl keys
    let mut builder = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);

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

    // create client
    let client = Client::builder()
        .connector::<&str>(Connector::default().openssl(builder.build()))
        .build(SharedCfg::default())
        .await
        .unwrap();

    // Create request builder, configure request and send
    let response = client
        .get("https://127.0.0.1:8443/")
        .header("User-Agent", "ntex")
        .send()
        .await?;

    // server http response
    println!("Response: {:?}", response);

    // read response body
    let body = response.body().await.unwrap();
    println!("Downloaded: {:?} bytes", body.len());

    Ok(())
}
