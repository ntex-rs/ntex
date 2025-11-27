use futures::{Future, future};
use ntex::http::{Method, StatusCode};
use ntex::web::{App, Error, HttpResponse, HttpResponseBuilder, test, types::Path};
use ntex_macros::{
    web_connect, web_delete, web_get, web_head, web_options, web_patch, web_post, web_put,
    web_trace,
};

// Make sure that we can name function as 'config'
#[web_get("/config")]
async fn config() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_get("/test")]
async fn test_handler() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_put("/test")]
async fn put_test() -> HttpResponse {
    HttpResponse::Created().finish()
}

#[web_patch("/test")]
async fn patch_test() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_post("/test")]
async fn post_test() -> HttpResponse {
    HttpResponse::NoContent().finish()
}

#[web_head("/test")]
async fn head_test() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_connect("/test")]
async fn connect_test() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_options("/test")]
async fn options_test() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_trace("/test")]
async fn trace_test() -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[web_get("/test")]
fn auto_async() -> impl Future<Output = Result<HttpResponse, Error>> {
    future::ok(HttpResponse::Ok().finish())
}

#[web_put("/test/{param}")]
async fn put_param_test(_: Path<String>) -> HttpResponseBuilder {
    HttpResponse::Created()
}

#[web_delete("/test/{param}")]
async fn delete_param_test(_: Path<String>) -> HttpResponseBuilder {
    HttpResponse::NoContent()
}

#[web_get("/test/{param}")]
async fn get_param_test(_: Path<String>) -> HttpResponse {
    HttpResponse::Ok().finish()
}

#[ntex::test]
async fn test_params() {
    let srv = test::server(|| {
        App::new()
            .service(get_param_test)
            .service(put_param_test)
            .service(delete_param_test)
    })
    .await;

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
    let srv = test::server(|| {
        App::new()
            .service(post_test)
            .service(put_test)
            .service(head_test)
            .service(connect_test)
            .service(options_test)
            .service(trace_test)
            .service(patch_test)
            .service(test_handler)
    })
    .await;
    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::HEAD, srv.url("/test"));
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
    let srv = test::server(|| App::new().service(auto_async)).await;

    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}
