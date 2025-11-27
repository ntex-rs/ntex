use std::io;

use ntex::http::test::server as test_server;
use ntex::http::{HttpService, Method, Request, Response};
use ntex::service::ServiceFactory;
use ntex::{client::error::SendRequestError, time, util::Bytes, util::Ready};

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
        HttpService::new(|_| Ready::Ok::<_, io::Error>(Response::Ok().body(STR)))
    })
    .await;

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::GET, "/").header("x-test", "111").send();
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let mut response = srv.request(Method::POST, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_connection_close() {
    let srv = test_server(move || {
        HttpService::new(|_| Ready::Ok::<_, io::Error>(Response::Ok().body(STR)))
            .map(|_| ())
    })
    .await;

    let response = srv
        .request(Method::GET, "/")
        .force_close()
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_with_query_parameter() {
    let srv = test_server(move || {
        HttpService::new(|req: Request| {
            if req.uri().query().unwrap().contains("qp=") {
                Ready::Ok::<_, io::Error>(Response::Ok().finish())
            } else {
                Ready::Ok::<_, io::Error>(Response::BadRequest().finish())
            }
        })
        .map(|_| ())
    })
    .await;

    let request = srv.request(Method::GET, srv.url("/?qp=5"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_client_timeout() {
    let srv = test_server(move || {
        HttpService::new(|_| async {
            time::sleep(time::Seconds(10)).await;
            Ok::<_, io::Error>(Response::Ok().body(STR))
        })
        .map(|_| ())
    })
    .await
    .set_client_timeout(time::Seconds(1), time::Millis(30_000))
    .await;

    let err = srv
        .request(Method::GET, "/")
        .force_close()
        .send()
        .await
        .err()
        .unwrap();
    assert!(matches!(err, SendRequestError::Timeout));
}
