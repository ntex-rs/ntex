use ntex::web;

#[derive(Debug, thiserror::Error)]
#[error("my error")]
struct MyError;

fn main() {
    let _: web::WebError<(), web::DefaultError> = web::WebError::from_err(MyError);
}
