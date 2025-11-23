use ntex::http::client::{Client, error::SendRequestError};

#[ntex::main]
async fn main() -> Result<(), SendRequestError> {
    env_logger::init();

    let client = Client::new();

    // Create request builder, configure request and send
    let mut response = client
        .get("https://www.rust-lang.org/")
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
