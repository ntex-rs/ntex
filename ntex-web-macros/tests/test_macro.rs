use ntex::http::{Error, StatusCode, Method};
use ntex::web::{test, types::Path, App, HttpResponse, Responder};
use ntex_web_macros::{connect, delete, get, head, options, patch, post, put, trace};
use futures::{future, Future};

// Make sure that we can name function as 'config'
#[get("/config")]
async fn config() -> impl Responder {
    HttpResponse::Ok()
}

#[get("/test")]
async fn test_handler() -> impl Responder {
    HttpResponse::Ok()
}

#[put("/test")]
async fn put_test() -> impl Responder {
    HttpResponse::Created()
}

#[patch("/test")]
async fn patch_test() -> impl Responder {
    HttpResponse::Ok()
}

#[post("/test")]
async fn post_test() -> impl Responder {
    HttpResponse::NoContent()
}

#[head("/test")]
async fn head_test() -> impl Responder {
    HttpResponse::Ok()
}

#[connect("/test")]
async fn connect_test() -> impl Responder {
    HttpResponse::Ok()
}

#[options("/test")]
async fn options_test() -> impl Responder {
    HttpResponse::Ok()
}

#[trace("/test")]
async fn trace_test() -> impl Responder {
    HttpResponse::Ok()
}

#[get("/test")]
fn auto_async() -> impl Future<Output = Result<HttpResponse, Error>> {
    future::ok(HttpResponse::Ok().finish())
}

#[get("/test")]
fn auto_sync() -> impl Future<Output = Result<HttpResponse, Error>> {
    future::ok(HttpResponse::Ok().finish())
}

#[put("/test/{param}")]
async fn put_param_test(_: Path<String>) -> impl Responder {
    HttpResponse::Created()
}

#[delete("/test/{param}")]
async fn delete_param_test(_: Path<String>) -> impl Responder {
    HttpResponse::NoContent()
}

#[get("/test/{param}")]
async fn get_param_test(_: Path<String>) -> impl Responder {
    HttpResponse::Ok()
}

#[ntex::test]
async fn test_params() {
    let srv = test::start(|| {
        App::new()
            .service(get_param_test)
            .service(put_param_test)
            .service(delete_param_test)
    });

    let request = srv.request(Method::GET, srv.url("/test/it"));
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let request = srv.request(Method::PUT, srv.url("/test/it"));
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let request = srv.request(Method::DELETE, srv.url("/test/it"));
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[ntex::test]
async fn test_body() {
    let srv = test::start(|| {
        App::new()
            .service(post_test)
            .service(put_test)
            .service(head_test)
            .service(connect_test)
            .service(options_test)
            .service(trace_test)
            .service(patch_test)
            .service(test_handler)
    });
    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::HEAD, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::CONNECT, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::OPTIONS, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::TRACE, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::PATCH, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::PUT, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.status(), StatusCode::CREATED);

    let request = srv.request(Method::POST, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_auto_async() {
    let srv = test::start(|| App::new().service(auto_async));

    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}
