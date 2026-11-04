use ntex::web;

struct NotExtractor;

async fn index(_: NotExtractor) -> &'static str {
    "Hello"
}

fn main() {
    let _ = web::App::default().route("/", web::get().to(index));
}
