use ntex::web;

fn index() -> &'static str {
    "Hello"
}

fn main() {
    let _ = web::App::default().route("/", web::get().to(index));
}
