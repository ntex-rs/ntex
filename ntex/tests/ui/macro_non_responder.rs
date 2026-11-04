use ntex::web;

struct NotResponder;

#[web::get("/")]
async fn index() -> NotResponder {
    NotResponder
}

fn main() {
    let _ = web::App::default().service(index);
}
