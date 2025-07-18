use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use coo_kie::Cookie;
use flate2::{read::GzDecoder, write::GzEncoder, write::ZlibEncoder, Compression};
use rand::Rng;

use ntex::http::client::error::SendRequestError;
use ntex::http::client::{Client, Connector};
use ntex::http::test::server as test_server;
use ntex::http::{header, HttpMessage, HttpService};
use ntex::service::{chain_factory, map_config};
use ntex::web::dev::AppConfig;
use ntex::web::middleware::Compress;
use ntex::web::{self, test, App, BodyEncoding, Error, HttpRequest, HttpResponse};
use ntex::{time::sleep, time::Millis, time::Seconds, util::Bytes, util::Ready};

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
async fn test_simple() {
    let srv = test::server(|| {
        App::new().service(
            web::resource("/").route(web::to(|| async { HttpResponse::Ok().body(STR) })),
        )
    });

    let request = srv.get("/").header("x-test", "111").send();
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let mut response = srv.post("/").timeout(Seconds(30)).send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_freeze() {
    let srv = test::server(|| {
        App::new().service(
            web::resource("/").route(web::to(|| async { HttpResponse::Ok().body(STR) })),
        )
    });

    // prep request
    let request = srv.get("/").header("x-test", "111").freeze().unwrap();

    // send one request
    let mut response = request.send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // send one request
    let mut response = request.send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // send one request
    let mut response = request.extra_header("x-test2", "222").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_json() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(
            |_: web::types::Json<String>| async { HttpResponse::Ok() },
        )))
    });

    let request = srv
        .get("/")
        .header("x-test", "111")
        .send_json(&"TEST".to_string());
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // same with frozen request
    let request = srv.get("/").header("x-test", "111").freeze().unwrap();

    let response = request.send_json(&"TEST".to_string()).await.unwrap();
    assert!(response.status().is_success());

    let response = request
        .extra_header("x-test2", "112")
        .send_json(&"TEST".to_string())
        .await
        .unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_form() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(
            |_: web::types::Form<HashMap<String, String>>| async { HttpResponse::Ok() },
        )))
    });

    let mut data = HashMap::new();
    let _ = data.insert("key".to_string(), "TEST".to_string());

    let request = srv.get("/").header("x-test", "111").send_form(&data);
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // same with frozen request
    let request = srv.get("/").header("x-test", "111").freeze().unwrap();
    let response = request.send_form(&data).await.unwrap();
    assert!(response.status().is_success());

    let response = request
        .extra_header("x-test2", "112")
        .send_form(&data)
        .await
        .unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_timeout() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|| async {
            sleep(Millis(2000)).await;
            HttpResponse::Ok().body(STR)
        })))
    });

    let connector = Connector::default()
        .connector(ntex::connect::Connector::new())
        .timeout(Seconds(15))
        .finish();

    let client = Client::build()
        .connector(connector)
        .timeout(Seconds(1))
        .finish();

    let request = client.get(srv.url("/")).send();
    match request.await {
        Err(SendRequestError::Timeout) => (),
        _ => panic!(),
    }
}

#[ntex::test]
async fn test_timeout_override() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|| async {
            sleep(Millis(2000)).await;
            HttpResponse::Ok().body(STR)
        })))
    });

    let client = Client::build().timeout(Seconds(50)).finish();
    let request = client.get(srv.url("/")).timeout(Seconds(1)).send();
    match request.await {
        Err(SendRequestError::Timeout) => (),
        _ => panic!(),
    }
}

#[ntex::test]
async fn test_connection_reuse() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(HttpService::new(map_config(
            App::new().service(
                web::resource("/").route(web::to(|| async { HttpResponse::Ok() })),
            ),
            |_| AppConfig::default(),
        )))
    });

    let client = Client::build().timeout(Seconds(30)).finish();

    // req 1
    let request = client.get(srv.url("/")).send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let req = client.post(srv.url("/"));
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // one connection
    assert_eq!(num.load(Ordering::Relaxed), 1);
}

#[ntex::test]
async fn test_connection_force_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(HttpService::new(map_config(
            App::new().service(
                web::resource("/").route(web::to(|| async { HttpResponse::Ok() })),
            ),
            |_| AppConfig::default(),
        )))
    });

    let client = Client::build().timeout(Seconds(30)).finish();

    // req 1
    let request = client.get(srv.url("/")).force_close().send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let client = Client::build().timeout(Seconds(30)).finish();
    let req = client.post(srv.url("/")).force_close();
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // two connection
    assert_eq!(num.load(Ordering::Relaxed), 2);
}

#[ntex::test]
async fn test_connection_server_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(HttpService::new(map_config(
            App::new().service(web::resource("/").route(web::to(|| async {
                HttpResponse::Ok().force_close().finish()
            }))),
            |_| AppConfig::default(),
        )))
    });

    let client = Client::build().timeout(Seconds(30)).finish();

    // req 1
    let request = client.get(srv.url("/")).send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let req = client.post(srv.url("/"));
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // two connection
    assert_eq!(num.load(Ordering::Relaxed), 2);
}

#[ntex::test]
async fn test_connection_wait_queue() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(HttpService::new(map_config(
            App::new().service(
                web::resource("/")
                    .route(web::to(|| async { HttpResponse::Ok().body(STR) })),
            ),
            |_| AppConfig::default(),
        )))
    });

    let client = Client::build()
        .timeout(Seconds(30))
        .connector(Connector::default().limit(1).finish())
        .finish();

    // req 1
    let request = client.get(srv.url("/")).send();
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let req2 = client.post(srv.url("/"));
    let req2_fut = req2.send();

    // read response 1
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // req 2
    let response = req2_fut.await.unwrap();
    assert!(response.status().is_success());

    // two connection
    assert_eq!(num.load(Ordering::Relaxed), 1);
}

#[ntex::test]
async fn test_connection_wait_queue_force_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(move || {
        let num2 = num2.clone();
        chain_factory(move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ready::Ok(io)
        })
        .and_then(HttpService::new(map_config(
            App::new().service(web::resource("/").route(web::to(|| async {
                HttpResponse::Ok().force_close().body(STR)
            }))),
            |_| AppConfig::default(),
        )))
    });

    let client = Client::build()
        .timeout(Seconds(30))
        .connector(Connector::default().limit(1).finish())
        .finish();

    // req 1
    let request = client.get(srv.url("/")).send();
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let req2 = client.post(srv.url("/"));
    let req2_fut = req2.send();

    // read response 1
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // req 2
    let response = req2_fut.await.unwrap();
    assert!(response.status().is_success());

    // two connection
    assert_eq!(num.load(Ordering::Relaxed), 2);
}

#[ntex::test]
async fn test_with_query_parameter() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").to(|req: HttpRequest| async move {
            if req.query_string().contains("qp") {
                HttpResponse::Ok()
            } else {
                HttpResponse::BadRequest()
            }
        }))
    });

    let res = srv.get("/?qp=5").send().await.unwrap();
    assert!(res.status().is_success());
}

#[ntex::test]
async fn test_no_decompress() {
    let srv = test::server(|| {
        App::new()
            .wrap(Compress::default())
            .service(web::resource("/").route(web::to(|| async {
                let mut res = HttpResponse::Ok().body(STR);
                res.encoding(header::ContentEncoding::Gzip);
                res
            })))
    });

    let client = Client::build().timeout(Seconds(30)).finish();

    let mut res = client
        .get(srv.url("/"))
        .no_decompress()
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    // read response
    let bytes = res.body().await.unwrap();

    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));

    // POST
    let mut res = client
        .post(srv.url("/"))
        .no_decompress()
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    let bytes = res.body().await.unwrap();
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_client_gzip_encoding() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|| async {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(STR.as_ref()).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "gzip")
                .body(data)
        })))
    });

    // client request
    let mut response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_client_gzip_encoding_large() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|| async {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(STR.repeat(10).as_ref()).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "gzip")
                .body(data)
        })))
    });

    // client request
    let mut response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from(STR.repeat(10)));
}

#[ntex::test]
async fn test_client_gzip_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(1_048_500)
        .map(char::from)
        .collect::<String>();

    let srv = test::server(|| {
        App::new()
            .state(web::types::PayloadConfig::default().limit(1_048_576))
            .service(web::resource("/").route(web::to(|data: Bytes| async move {
                let mut e = GzEncoder::new(Vec::new(), Compression::default());
                e.write_all(&data).unwrap();
                let data = e.finish().unwrap();
                HttpResponse::Ok()
                    .header("content-encoding", "gzip")
                    .body(data)
            })))
    });

    // client request
    let mut response = srv.post("/").send_body(data.clone()).await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().limit(1_048_576).await.unwrap();
    assert_eq!(bytes, Bytes::from(data));
}

#[ntex::test]
async fn test_client_deflate_encoding() {
    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|data: Bytes| async move {
            let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&data).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "deflate")
                .body(data)
        })))
    });

    // client request
    let mut response = srv.post("/").send_body(STR).await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_client_deflate_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(70_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server(|| {
        App::new().service(web::resource("/").route(web::to(|data: Bytes| async move {
            let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&data).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "deflate")
                .body(data)
        })))
    });

    // client request
    let mut response = srv.post("/").send_body(data.clone()).await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from(data));
}

// #[ntex::test]
// async fn test_body_streaming_implicit() {
//     let srv = test::TestServer::start(|app| {
//         app.handler(|_| {
//             let body = once(Ok(Bytes::from_static(STR.as_ref())));
//             HttpResponse::Ok()
//                 .content_encoding(http::ContentEncoding::Gzip)
//                 .body(Body::Streaming(Box::new(body)))
//         })
//     });

//     let request = srv.get("/").finish().unwrap();
//     let response = srv.execute(request.send()).unwrap();
//     assert!(response.status().is_success());

//     // read response
//     let bytes = srv.execute(response.body()).unwrap();
//     assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
// }

#[ntex::test]
async fn test_client_cookie_handling() {
    use std::io::{Error as IoError, ErrorKind};

    let cookie1 = Cookie::build(("cookie1", "value1"));
    let cookie2 = Cookie::build(("cookie2", "value2"))
        .domain("www.example.org")
        .path("/")
        .secure(true)
        .http_only(true);
    // Q: are all these clones really necessary? A: Yes, possibly
    let cookie1b = cookie1.clone();
    let cookie2b = cookie2.clone();

    let srv = test::server(move || {
        let cookie1 = cookie1b.clone();
        let cookie2 = cookie2b.clone();

        App::new().route(
            "/",
            web::to(web::dev::__assert_handler1(move |req: HttpRequest| {
                let cookie1 = cookie1.clone();
                let cookie2 = cookie2.clone();

                async move {
                    // Check cookies were sent correctly
                    let res: Result<(), Error> = req
                        .cookie("cookie1")
                        .ok_or(())
                        .and_then(|c1| {
                            if c1.value() == "value1" {
                                Ok(())
                            } else {
                                Err(())
                            }
                        })
                        .and_then(|()| req.cookie("cookie2").ok_or(()))
                        .and_then(|c2| {
                            if c2.value() == "value2" {
                                Ok(())
                            } else {
                                Err(())
                            }
                        })
                        .map_err(|_| Error::new(IoError::from(ErrorKind::NotFound)));

                    res?;

                    // Send some cookies back
                    Ok::<_, Error>(
                        HttpResponse::Ok().cookie(cookie1).cookie(cookie2).finish(),
                    )
                }
            })),
        )
    });

    let request = srv.get("/").cookie(cookie1.clone()).cookie(cookie2.clone());
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
    let c1 = response.cookie("cookie1").expect("Missing cookie1");
    assert_eq!(c1, cookie1);
    let c2 = response.cookie("cookie2").expect("Missing cookie2");
    assert_eq!(c2, cookie2);
}

#[ntex::test]
async fn client_read_until_eof() {
    let addr = ntex::server::TestServer::unused_addr();

    std::thread::spawn(move || {
        let lst = std::net::TcpListener::bind(addr).unwrap();

        for stream in lst.incoming() {
            if let Ok(mut stream) = stream {
                let mut b = [0; 1000];
                log::debug!("Reading request");
                let res = stream.read(&mut b).unwrap();
                log::debug!("Read {res:?}");
                let res = stream
                    .write_all(b"HTTP/1.0 200 OK\r\nconnection: close\r\n\r\nwelcome!");
                log::debug!("Sent {res:?}");
            } else {
                break;
            }
        }
    });
    sleep(Millis(500)).await;

    // client request
    let req = Client::build()
        .timeout(Seconds(30))
        .finish()
        .get(format!("http://{addr}/").as_str());
    let mut response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"welcome!"));
}

#[ntex::test]
async fn client_basic_auth() {
    let srv = test::server(|| {
        App::new().route(
            "/",
            web::to(|req: HttpRequest| async move {
                if req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    == "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
                {
                    HttpResponse::Ok()
                } else {
                    HttpResponse::BadRequest()
                }
            }),
        )
    });

    // set authorization header to Basic <base64 encoded username:password>
    let request = srv.get("/").basic_auth("username", Some("password"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn client_bearer_auth() {
    let srv = test::server(|| {
        App::new().route(
            "/",
            web::to(|req: HttpRequest| async move {
                if req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    == "Bearer someS3cr3tAutht0k3n"
                {
                    HttpResponse::Ok()
                } else {
                    HttpResponse::BadRequest()
                }
            }),
        )
    });

    // set authorization header to Bearer <token>
    let request = srv.get("/").bearer_auth("someS3cr3tAutht0k3n");
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}
