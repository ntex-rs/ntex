use std::convert::Infallible;

use futures::{Future, future};
use ntex::http::{Method, StatusCode};
use ntex::rt::System;
use ntex::web::{App, HttpResponse, HttpResponseBuilder, test, types::Path};
use ntex_macros::{
    web_connect, web_delete, web_get, web_head, web_options, web_patch, web_post, web_put,
    web_trace,
};

// Make sure that we can name function as 'config'
#[web_get("/config")]
async fn config() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_get("/test")]
async fn test_handler() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_put("/test")]
async fn put_test() -> HttpResponse {
    HttpResponse::Created().build()
}

#[web_patch("/test")]
async fn patch_test() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_post("/test")]
async fn post_test() -> HttpResponse {
    HttpResponse::NoContent().build()
}

#[web_head("/test")]
async fn head_test() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_connect("/test")]
async fn connect_test() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_options("/test")]
async fn options_test() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_trace("/test")]
async fn trace_test() -> HttpResponse {
    HttpResponse::Ok().build()
}

#[web_get("/test")]
fn auto_async() -> impl Future<Output = Result<HttpResponse, Infallible>> {
    future::ok(HttpResponse::Ok().build())
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
    HttpResponse::Ok().build()
}

#[ntex::main(name = "test", signals = false, ping_interval = 25)]
async fn main_with_ping_interval() {
    ntex::time::sleep(std::time::Duration::from_millis(100)).await;

    System::list_arbiter_pings(|_, recs| assert!(!recs.is_empty()));
}

#[test]
fn test_main_with_args() {
    main_with_ping_interval();
}

#[ntex::main(timer_resolution = 2)]
async fn main_with_timer_resolution() -> u8 {
    System::current().timer_resolution()
}

/// STATEMENT: The `timer_resolution` argument of `main` configures the system timer resolution.
///
/// HOW:
/// - Run a `main` function declared with `timer_resolution = 2`.
/// - Check the running system reports a 2 ms resolution.
#[test]
fn test_main_with_timer_resolution() {
    assert_eq!(main_with_timer_resolution(), 2);
}

#[ntex::test]
async fn test_params() {
    let srv = test::server(async |_| {
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

/// STATEMENT: Route macros infer handler arity after `cfg` and `cfg_attr` remove
/// parameters, while retaining enabled extractors.
///
/// HOW:
/// - Register a handler whose only parameter is disabled and one mixing enabled
///   extractors with disabled, undefined parameter types.
/// - Include active, inactive and nested `cfg_attr` conditions.
/// - Send requests and assert the zero-argument response and extracted path values.
#[ntex::test]
async fn test_conditional_params() {
    #[web_get("/zero")]
    async fn zero(#[cfg(any())] _: Undefined) -> &'static str {
        "zero"
    }

    #[web_get("/mixed/{value}")]
    async fn mixed(
        #[cfg(test)] value: Path<String>,
        #[cfg(any())] _: Undefined,
        #[cfg_attr(test, cfg(any()))] _: Undefined,
        #[cfg_attr(test, cfg_attr(test, cfg(any())))] _: Undefined,
        #[cfg_attr(not(test), cfg(any()))] req: ntex::web::HttpRequest,
    ) -> String {
        format!("{}:{}", value.into_inner(), req.path())
    }

    let app = test::init_service(App::new().service(zero).service(mixed)).await;
    for (uri, expected) in [("/zero", "zero"), ("/mixed/value", "value:/mixed/value")] {
        let req = test::TestRequest::with_uri(uri).to_request();
        let res = test::call_service(&app, req).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(test::read_body(res).await, expected);
    }
}

/// STATEMENT: Disabled parameters do not count toward the route handler argument limit.
///
/// HOW:
/// - Register a handler with sixteen disabled parameters and one enabled `Path` extractor.
/// - Send a request and assert that the enabled extractor supplies the response body.
#[ntex::test]
async fn test_conditional_params_argument_limit() {
    #[web_get("/limit/{value}")]
    async fn handler(
        #[cfg(any())] _p1: Undefined,
        #[cfg(any())] _p2: Undefined,
        #[cfg(any())] _p3: Undefined,
        #[cfg(any())] _p4: Undefined,
        #[cfg(any())] _p5: Undefined,
        #[cfg(any())] _p6: Undefined,
        #[cfg(any())] _p7: Undefined,
        #[cfg(any())] _p8: Undefined,
        #[cfg(any())] _p9: Undefined,
        #[cfg(any())] _p10: Undefined,
        #[cfg(any())] _p11: Undefined,
        #[cfg(any())] _p12: Undefined,
        #[cfg(any())] _p13: Undefined,
        #[cfg(any())] _p14: Undefined,
        #[cfg(any())] _p15: Undefined,
        #[cfg(any())] _p16: Undefined,
        value: Path<String>,
    ) -> String {
        value.into_inner()
    }

    let app = test::init_service(App::new().service(handler)).await;
    let req = test::TestRequest::with_uri("/limit/ok").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(test::read_body(res).await, "ok");
}

#[ntex::test]
async fn test_body() {
    let srv = test::server(async |_| {
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
    let srv = test::server(async |_| App::new().service(auto_async));

    let request = srv.request(Method::GET, srv.url("/test"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}

// A one-line body must not trigger `unused_braces`, rustfmt would split it
#[rustfmt::skip]
#[ntex::main]
async fn main_single_expr() -> Result<u32, ()> { Ok(1) }

#[test]
fn test_main_single_expr() {
    assert_eq!(main_single_expr(), Ok(1));
}

#[rustfmt::skip]
#[ntex::test]
async fn test_single_expr() -> Result<(), ()> { Ok(()) }
