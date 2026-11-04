use ntex::web;

struct NotExtractor;

#[web::get("/")]
async fn index(_: NotExtractor) -> &'static str {
    "Hello"
}

fn main() {
    let _ = web::App::default().service(index);
}
