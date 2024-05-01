use ntex::http::client::{error::SendRequestError, Client};

#[ntex::main]
async fn main() -> Result<(), SendRequestError> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();

    let client = Client::new();

    // Create request builder, configure request and send
    loop {
        let _response = client.get("https://www.google.com").send().await.unwrap();
        ntex::time::sleep(std::time::Duration::from_secs(10)).await;
    }

    Ok(())
}
