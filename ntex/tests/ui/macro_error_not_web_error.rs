use ntex::web;

#[derive(Debug)]
struct MyError;

#[web::get("/")]
async fn index() -> Result<String, MyError> {
    Err(MyError)
}

fn main() {
    let _ = web::App::default().service(index);
}
