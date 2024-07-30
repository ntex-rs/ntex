use ntex::web;

#[derive(serde::Deserialize)]
struct Info {
    username: String,
}

async fn submit(info: web::types::Json<Info>) -> Result<String, web::Error> {
    Ok(format!("Welcome {}!", info.username))
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    std::env::set_var("RUST_LOG", "trace");
    env_logger::init();
    web::HttpServer::new(|| {
        let json_config = web::types::JsonConfig::default().limit(4096);

        web::App::new().service(
            web::resource("/")
                .state(json_config)
                .route(web::post().to(submit)),
        )
    })
    .bind(("127.0.0.1", 8080))?
    .workers(1)
    .run()
    .await
}
