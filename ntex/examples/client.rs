//! Minimal HTTPS client that prints response metadata and a body preview.

use ntex::client::Client;

#[ntex::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let url = "https://www.rust-lang.org/";
    let client = Client::new();
    let response = client
        .get(url)
        .header("user-agent", "ntex-example")
        .send()
        .await?;

    println!("GET {url} -> {}", response.status());
    println!("response headers: {}", response.headers().len());

    let body = response.body().await?;
    let preview_len = body.len().min(120);
    println!("downloaded {} bytes", body.len());
    println!(
        "body preview: {}",
        String::from_utf8_lossy(&body[..preview_len])
    );
    Ok(())
}
