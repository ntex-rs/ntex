use ntex::web;

#[derive(Clone)]
struct Config;

fn main() {
    let _ = web::App::<Config>::new();
}
