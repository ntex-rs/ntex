use std::io;

use bytes::Bytes;
use futures::future::{self, ok};

use ntex::http::test::server as test_server;
use ntex::http::{HttpService, Method, Request, Response};
use ntex::service::ServiceFactory;

const STR: &str = "Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World \
                   Hello World Hello World Hello World Hello World Hello World";

#[ntex::test]
async fn test_h1_v2() {
    let srv = test_server(move || {
        HttpService::build()
            .finish(|_| future::ok::<_, io::Error>(Response::Ok().body(STR)))
            .tcp()
    });

    let response = srv.get("/").send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.get("/").header("x-test", "111").send();
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let mut response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_connection_close() {
    let srv = test_server(move || {
        HttpService::build()
            .finish(|_| ok::<_, io::Error>(Response::Ok().body(STR)))
            .tcp()
            .map(|_| ())
    });

    let response = srv.get("/").force_close().send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_with_query_parameter() {
    let srv = test_server(move || {
        HttpService::build()
            .finish(|req: Request| {
                if req.uri().query().unwrap().contains("qp=") {
                    ok::<_, io::Error>(Response::Ok().finish())
                } else {
                    ok::<_, io::Error>(Response::BadRequest().finish())
                }
            })
            .tcp()
            .map(|_| ())
    });

    let request = srv.request(Method::GET, srv.url("/?qp=5"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}
