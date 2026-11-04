use ntex::web;

async fn index(_: u32) -> &'static str {
    "Hello"
}

fn main() {
    let _ = web::App::default().route("/", web::get().to_with_state(index));
}
