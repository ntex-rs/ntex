use ntex::web;

struct NotResponder;

async fn index() -> NotResponder {
    NotResponder
}

fn main() {
    let _ = web::App::default().route("/", web::get().to(index));
}
