use ntex::web;

#[derive(Debug, thiserror::Error)]
#[error("my error")]
struct MyError;

async fn index() -> Result<&'static str, MyError> {
    Err(MyError)
}

fn main() {
    let _ = web::App::default().route("/", web::get().to(index));
}
