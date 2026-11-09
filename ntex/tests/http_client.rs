use std::sync::atomic::{AtomicUsize, Ordering};
use std::{cell::RefCell, collections::HashMap, io, io::Read, io::Write, net, rc::Rc, sync::Arc};

use coo_kie::Cookie;
use flate2::{Compression, read::GzDecoder, write::GzEncoder, write::ZlibEncoder};
use rand::Rng;

use ntex::client::{Client, ClientConfig, error::ClientError};
use ntex::http::test::server as test_server;
use ntex::http::{HttpMessage, HttpService, Method, Response, header};
use ntex::io::IoConfig;
use ntex::service::{cfg::SharedCfg, fn_layer, service};
use ntex::web::middleware::Compress;
use ntex::web::{self, App, BodyEncoding, HttpRequest, HttpResponse, test};
use ntex::{client, time::Millis, time::Seconds, time::sleep, util::Bytes};

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

/// STATEMENT: The client receives complete bodies for GET and POST requests.
///
/// HOW:
/// - Serve `STR`, send GET with a custom header and POST with a request timeout.
/// - Assert both statuses succeed and both bodies equal `STR`.
#[ntex::test]
async fn test_simple() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok().body(STR))))
    });

    let request = srv.get("/").header("x-test", "111").send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let response = srv.post("/").timeout(Seconds(30)).send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: `send_json` produces a body accepted by the JSON extractor.
///
/// HOW:
/// - Serve a route extracting `Json<String>` and send a string with `send_json`.
/// - Assert the response status is successful.
#[ntex::test]
async fn test_json() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(
            async |_: web::types::Json<String>| HttpResponse::Ok(),
        )))
    });

    let response = srv
        .get("/")
        .header("x-test", "111")
        .send_json(&"TEST".to_string())
        .await
        .unwrap();
    assert!(response.status().is_success());
}

/// STATEMENT: `send_form` produces a body accepted by the form extractor.
///
/// HOW:
/// - Serve a route extracting `Form<HashMap<String, String>>` and send a map containing `key`.
/// - Assert the response status is successful.
#[ntex::test]
async fn test_form() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(
            async |_: web::types::Form<HashMap<String, String>>| HttpResponse::Ok(),
        )))
    });

    let mut data = HashMap::new();
    let _ = data.insert("key".to_string(), "TEST".to_string());

    let request = srv.get("/").header("x-test", "111").send_form(&data);
    let response = request.await.unwrap();
    assert!(response.status().is_success());
}

/// STATEMENT: A configured response timeout fails a request whose handler is too slow.
///
/// HOW:
/// - Delay the response five seconds and configure a 300 ms response timeout.
/// - Send GET and assert `ClientError::Timeout`.
#[ntex::test]
async fn test_timeout() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            sleep(Millis(5000)).await;
            HttpResponse::Ok().body(STR)
        })))
    });

    let client = Client::with_config(
        SharedCfg::new("SVC")
            .add(IoConfig::new().set_connect_timeout(2500))
            .add(ClientConfig::new().set_response_timeout(Millis(300))),
    );

    let err = client.get(srv.url("/")).send().await.err().unwrap();
    assert!(matches!(err.into_error(), ClientError::Timeout));
}

/// STATEMENT: A request-specific timeout overrides a longer client-wide timeout.
///
/// HOW:
/// - Delay the handler two seconds and configure a 50-second client timeout.
/// - Set a 200 ms request timeout and assert `ClientError::Timeout`.
#[ntex::test]
async fn test_timeout_override() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            sleep(Millis(2000)).await;
            HttpResponse::Ok().body(STR)
        })))
    });

    let client = Client::with_config(ClientConfig::new().set_response_timeout(Seconds(50)));

    let err = client
        .get(srv.url("/"))
        .timeout(Millis(200))
        .send()
        .await
        .err();
    assert!(matches!(err.unwrap().into_error(), ClientError::Timeout));
}

/// STATEMENT: A zero request timeout disables the client-wide response timeout.
///
/// HOW:
/// - Configure a 100 ms response timeout against a handler delayed 500 ms.
/// - Override with `Millis::ZERO` and assert a successful response.
#[ntex::test]
async fn test_timeout_disable_override() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            sleep(Millis(500)).await;
            HttpResponse::Ok().body(STR)
        })))
    });

    let client = Client::with_config(ClientConfig::new().set_response_timeout(Millis(100)));

    // zero disables the client-wide timeout for this request
    let res = client
        .get(srv.url("/"))
        .timeout(Millis::ZERO)
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
}

/// STATEMENT: The response-header and response-payload timeouts are independent phases:
/// disabling the header timeout leaves the payload timeout active, and the header timeout never
/// bounds reading the body.
///
/// HOW:
/// - Serve a response whose head is delayed 300 ms and whose single body chunk is delayed
///   another 300 ms.
/// - With a 100 ms header timeout, assert `send()` fails with `ClientError::Timeout`.
/// - After `disable_timeout()` with a 100 ms payload timeout, assert `send()` succeeds but
///   `body()` fails with a `TimedOut` payload error.
/// - With a 2 s header timeout and a zero payload timeout, assert `body()` returns the data
///   although reading it outlasts the header timeout.
#[ntex::test]
async fn test_timeout_phases_are_independent() {
    use futures_util::stream;
    use ntex::http::error::PayloadError;

    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            sleep(Millis(300)).await;
            HttpResponse::Ok().streaming(stream::once(Box::pin(async {
                sleep(Millis(300)).await;
                Ok::<_, io::Error>(Bytes::from_static(b"data"))
            })))
        })))
    });

    let cfg = || {
        ClientConfig::new()
            .set_response_timeout(Millis(100))
            .set_response_payload_timeout(Millis(100))
    };

    let client = Client::with_config(cfg());
    let err = client.get(srv.url("/")).send().await.unwrap_err();
    assert!(matches!(err.into_error(), ClientError::Timeout));

    let client = Client::with_config(cfg().disable_timeout());
    let res = client.get(srv.url("/")).send().await.unwrap();
    assert!(res.status().is_success());
    let err = res.body().await.unwrap_err();
    assert!(matches!(
        &**err,
        PayloadError::Incomplete(Some(e)) if e.kind() == io::ErrorKind::TimedOut
    ));

    let client = Client::with_config(
        cfg()
            .set_response_timeout(Seconds(2))
            .set_response_payload_timeout(Millis::ZERO),
    );
    let res = client.get(srv.url("/")).send().await.unwrap();
    assert_eq!(res.body().await.unwrap(), "data");
}

/// STATEMENT: Sequential GET and POST requests reuse an HTTP/1 connection.
///
/// HOW:
/// - Count accepted connections and send GET then POST with one client.
/// - Assert both statuses succeed and only one connection was accepted.
#[ntex::test]
async fn test_connection_reuse() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(
            App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok()))),
        ))
    });

    let client = Client::with_config(ClientConfig::new().set_response_timeout(Seconds(30)));

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

/// STATEMENT: A force-close request still receives a successful response.
///
/// HOW:
/// - Serve a body through `HttpService` and send GET with `force_close`.
/// - Assert the response status succeeds.
#[ntex::test]
async fn test_connection_close() {
    let srv = test_server(async move |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv
        .request(Method::GET, "/")
        .force_close()
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

/// STATEMENT: Force-close requests from separate clients use separate connections.
///
/// HOW:
/// - Count accepted connections and send one force-close request from each of two clients.
/// - Assert both responses succeed and the connection count is `2`.
#[ntex::test]
async fn test_connection_force_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(
            App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok()))),
        ))
    });

    let client = Client::with_config(ClientConfig::new().set_response_timeout(Seconds(30)));

    // req 1
    let request = client.get(srv.url("/")).force_close().send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // req 2
    let client = Client::with_config(ClientConfig::new().set_response_timeout(Seconds(30)));
    let req = client.post(srv.url("/")).force_close();
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // two connection
    assert_eq!(num.load(Ordering::Relaxed), 2);
}

/// STATEMENT: Server-forced closure prevents connection reuse.
///
/// HOW:
/// - Serve responses with `force_close` and count accepted connections.
/// - Send GET then POST from one client and assert successful responses on two connections.
#[ntex::test]
async fn test_connection_server_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(App::new().service(
            web::resource("/").route(web::to(async || HttpResponse::Ok().force_close().build())),
        )))
    });

    let client = Client::with_config(ClientConfig::new().set_response_timeout(Seconds(30)));

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

/// STATEMENT: A one-connection pool reuses its connection after the first payload is consumed.
///
/// HOW:
/// - Set the HTTP/1 connection limit to `1`, obtain the first response, and construct a second request future.
/// - Read and verify the first body, await the second response, and assert both succeed with one accepted connection.
#[ntex::test]
async fn test_connection_wait_queue() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(App::new().service(
            web::resource("/").route(web::to(async || HttpResponse::Ok().body(STR))),
        )))
    });

    let client = Client::with_config(
        ClientConfig::new()
            .set_response_timeout(Seconds(30))
            .set_h1_connection_limit(1),
    );

    // req 1
    let request = client.get(srv.url("/")).send();
    let response = request.await.unwrap();
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

/// STATEMENT: A one-connection pool opens a replacement after a force-close response.
///
/// HOW:
/// - Limit the pool to one connection while the server force-closes responses; construct a second request after obtaining the first response.
/// - Read and verify the first body, await the second response, and assert two accepted connections.
#[ntex::test]
async fn test_connection_wait_queue_force_close() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num2 = num2.clone();
        service(async move |io| {
            num2.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(App::new().service(
            web::resource("/").route(web::to(async || HttpResponse::Ok().force_close().body(STR))),
        )))
    });

    let client = Client::with_config(
        ClientConfig::new()
            .set_h1_connection_limit(1)
            .set_response_timeout(Seconds(30)),
    );

    // req 1
    let request = client.get(srv.url("/")).send();
    let response = request.await.unwrap();
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

/// STATEMENT: Relative and absolute request URLs retain query parameters.
///
/// HOW:
/// - Serve success only when `query_string` contains `qp`.
/// - Send `/?qp=5` through both request-building paths and assert successful responses.
#[ntex::test]
async fn test_with_query_parameter() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").to(async move |req: HttpRequest| {
            if req.query_string().contains("qp") {
                HttpResponse::Ok()
            } else {
                HttpResponse::BadRequest()
            }
        }))
    });

    let res = srv.get("/?qp=5").send().await.unwrap();
    assert!(res.status().is_success());

    let request = srv.request(Method::GET, srv.url("/?qp=5"));
    let response = request.send().await.unwrap();
    assert!(response.status().is_success());
}

/// STATEMENT: `no_decompress` preserves gzip bytes for GET and POST.
///
/// HOW:
/// - Serve gzip through `Compress` and disable decompression on both requests.
/// - Assert successful statuses, manually decode each body with `GzDecoder`, and compare with `STR`.
#[ntex::test]
async fn test_no_decompress() {
    let srv = test::server(async |_| {
        App::new()
            .middleware(Compress::default())
            .service(web::resource("/").route(web::to(async || {
                let mut res = HttpResponse::Ok().body(STR);
                res.encoding(header::ContentEncoding::Gzip);
                res
            })))
    });

    let client = Client::builder().build(ClientConfig::new().set_response_timeout(Seconds(30)));

    let res = client
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
    let res = client
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

/// STATEMENT: The client advertises supported encodings and transparently decodes zstd.
///
/// HOW:
/// - Assert the request advertises `gzip, deflate, zstd` and return a zstd-encoded `STR`.
/// - Assert success, removal of `content-encoding`, and the decoded body equals `STR`.
#[ntex::test]
async fn test_client_zstd_encoding() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async |req: HttpRequest| {
            assert_eq!(
                req.headers().get(header::ACCEPT_ENCODING).unwrap(),
                "gzip, deflate, zstd"
            );
            HttpResponse::Ok()
                .header("content-encoding", "zstd")
                .body(zstd::encode_all(STR.as_bytes(), 0).unwrap())
        })))
    });

    let response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());
    assert!(!response.headers().contains_key("content-encoding"));
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: Automatic gzip decompression removes encoded-payload headers, while opt-out preserves them.
///
/// HOW:
/// - Return gzip-encoded `STR`; assert the default response has neither encoding nor length headers and its body equals `STR`.
/// - Repeat with `no_decompress` and assert the gzip header, advertised compressed length, and different body bytes.
#[ntex::test]
async fn test_client_gzip_encoding() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(STR.as_ref()).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "gzip")
                .body(data)
        })))
    });

    // client request
    let response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());
    // headers of the encoded payload are removed
    assert!(!response.headers().contains_key("content-encoding"));
    assert!(!response.headers().contains_key("content-length"));

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // encoded payload
    let response = srv.post("/").no_decompress().send().await.unwrap();
    assert_eq!(response.headers().get("content-encoding").unwrap(), "gzip");
    let len: usize = response
        .headers()
        .get("content-length")
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), len);
    assert_ne!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: The client decompresses a large repetitive gzip response.
///
/// HOW:
/// - Serve gzip-compressed `STR.repeat(10)` and send POST.
/// - Assert a successful response whose decoded body equals the original repeated string.
#[ntex::test]
async fn test_client_gzip_encoding_large() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || {
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(STR.repeat(10).as_ref()).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "gzip")
                .body(data)
        })))
    });

    // client request
    let response = srv.post("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from(STR.repeat(10)));
}

/// STATEMENT: The client decompresses a near-megabyte random gzip response within configured payload limits.
///
/// HOW:
/// - Generate 1,048,500 random characters, raise the request payload limit, and POST them to a gzip-encoding echo handler.
/// - Read with a 1 MiB response limit and assert success and exact original bytes.
#[ntex::test]
async fn test_client_gzip_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(1_048_500)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(
        test::config().server_cfg(
            SharedCfg::new("SRV").add(
                web::WebAppConfig::new()
                    .set_value(web::types::PayloadConfig::default().limit(1_048_576)),
            ),
        ),
        async |_| {
            App::new().service(web::resource("/").route(web::to(async move |data: Bytes| {
                let mut e = GzEncoder::new(Vec::new(), Compression::default());
                e.write_all(&data).unwrap();
                let data = e.finish().unwrap();
                HttpResponse::Ok()
                    .header("content-encoding", "gzip")
                    .body(data)
            })))
        },
    );

    // client request
    let response = srv.post("/").send_body(data.clone()).await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().limit(1_048_576).await.unwrap();
    assert_eq!(bytes, Bytes::from(data));
}

/// STATEMENT: The client transparently decompresses a deflate response.
///
/// HOW:
/// - POST `STR` to a handler that returns its zlib-compressed bytes with `content-encoding: deflate`.
/// - Assert success and a decoded response body equal to `STR`.
#[ntex::test]
async fn test_client_deflate_encoding() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async move |data: Bytes| {
            let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&data).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "deflate")
                .body(data)
        })))
    });

    // client request
    let response = srv.post("/").send_body(STR).await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: The client decompresses a large random deflate response.
///
/// HOW:
/// - POST 70,000 random characters to a zlib-encoding echo handler.
/// - Assert success and the decoded body matches the original data.
#[ntex::test]
async fn test_client_deflate_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(70_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async move |data: Bytes| {
            let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(&data).unwrap();
            let data = e.finish().unwrap();

            HttpResponse::Ok()
                .header("content-encoding", "deflate")
                .body(data)
        })))
    });

    // client request
    let response = srv.post("/").send_body(data.clone()).await.unwrap();
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

//     let request = srv.get("/").build().unwrap();
//     let response = srv.execute(request.send()).unwrap();
//     assert!(response.status().is_success());

//     // read response
//     let bytes = srv.execute(response.body()).unwrap();
//     assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
// }

/// STATEMENT: Request cookies reach the handler and response cookies retain their values and attributes.
///
/// HOW:
/// - Send two cookies, one with domain, path, secure, and HTTP-only attributes, and validate both values in the handler.
/// - Return both cookies and assert a successful status and equality with the originals.
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

    let srv = test::server(async move |_| {
        let cookie1 = cookie1b.clone();
        let cookie2 = cookie2b.clone();

        App::new().route(
            "/",
            web::to(web::dev::__assert_handler1(
                async move |req: HttpRequest| {
                    let cookie1 = cookie1.clone();
                    let cookie2 = cookie2.clone();

                    // Check cookies were sent correctly
                    let res: Result<(), io::Error> = req
                        .cookie("cookie1")
                        .ok_or(())
                        .and_then(|c1| if c1.value() == "value1" { Ok(()) } else { Err(()) })
                        .and_then(|()| req.cookie("cookie2").ok_or(()))
                        .and_then(|c2| if c2.value() == "value2" { Ok(()) } else { Err(()) })
                        .map_err(|_| IoError::from(ErrorKind::NotFound));

                    res?;

                    // Send some cookies back
                    Ok::<_, io::Error>(HttpResponse::Ok().cookie(cookie1).cookie(cookie2).build())
                },
            )),
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

/// STATEMENT: The HTTP test client applies its configured response deadline.
///
/// HOW:
/// - Delay the `HttpService` handler ten seconds and set the test client's timeout to one second.
/// - Send a force-close GET and assert `ClientError::Timeout`.
#[ntex::test]
async fn test_client_timeout() {
    let srv = test_server(async move |_| {
        HttpService::new(async |_| {
            sleep(Seconds(10)).await;
            Ok::<_, io::Error>(Response::Ok().body(STR))
        })
    })
    .set_client_timeout(Seconds(1), Millis(30_000));

    let err = srv
        .request(Method::GET, "/")
        .force_close()
        .send()
        .await
        .err()
        .unwrap();
    assert!(matches!(err.into_error(), ClientError::Timeout));
}

/// STATEMENT: The client reads an HTTP/1.0 body delimited by connection closure.
///
/// HOW:
/// - Run a raw TCP server returning an HTTP/1.0 response without a length and close the socket.
/// - Assert a successful status and the complete `welcome!` body.
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
                const S: &[u8] = b"HTTP/1.0 200 OK\r\nconnection: close\r\n\r\nwelcome!";
                let res = stream.write_all(S);
                log::debug!("Sent {res:?} cnt:{}", S.len());
                let _ = stream.shutdown(net::Shutdown::Both);
            } else {
                break;
            }
        }
    });
    sleep(Millis(250)).await;

    // client request
    let req = Client::builder()
        .build(ClientConfig::new().set_response_timeout(Seconds(30)))
        .get(format!("http://{addr}/").as_str());
    let response = req.send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"welcome!"));
}

/// Starts a raw server that answers every request with `response`, then closes.
fn raw_server(response: &'static [u8]) -> std::net::SocketAddr {
    let addr = ntex::server::TestServer::unused_addr();
    let lst = std::net::TcpListener::bind(addr).unwrap();
    std::thread::spawn(move || {
        for mut stream in lst.incoming().flatten() {
            let mut b = [0; 1000];
            let _ = stream.read(&mut b);
            let _ = stream.write_all(response);
            let _ = stream.shutdown(net::Shutdown::Both);
        }
    });
    addr
}

/// STATEMENT: The client reads an HTTP/1.1 body delimited by connection closure.
///
/// HOW:
/// - Use `raw_server` to send `connection: close` without a content length.
/// - Assert a successful response containing `welcome!`.
#[ntex::test]
async fn client_read_until_eof_http11() {
    let addr = raw_server(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nwelcome!");

    let response = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"welcome!"));
}

/// STATEMENT: A response body cut off by peer EOF inside a chunk header fails with `UnexpectedEof`.
///
/// HOW:
/// - Use `raw_server` to send one complete chunk and then a partial chunk-size line before closing.
/// - Read the body and assert `PayloadError::Incomplete` carries `io::ErrorKind::UnexpectedEof`.
#[ntex::test]
async fn client_payload_truncated_chunk_header() {
    use ntex::http::error::PayloadError;

    let addr = raw_server(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n1");

    let response = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let err = response.body().await.unwrap_err();
    assert!(
        matches!(
            &**err,
            PayloadError::Incomplete(Some(e)) if e.kind() == io::ErrorKind::UnexpectedEof
        ),
        "{err:?}"
    );
}

/// STATEMENT: A completed client payload remains exhausted when polled again.
///
/// HOW:
/// - Serve an eight-byte fixed-length response and take its undecoded payload stream.
/// - Assert the first chunk is `welcome!` and two subsequent receives both return `None`.
#[ntex::test]
async fn client_payload_poll_after_eof() {
    let addr = raw_server(b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\n\r\nwelcome!");

    let response = Client::new()
        .get(format!("http://{addr}/").as_str())
        .no_decompress()
        .send()
        .await
        .unwrap();
    let mut payload = response.take_payload();
    let chunk = ntex::util::stream_recv(&mut payload)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk, Bytes::from_static(b"welcome!"));
    assert!(ntex::util::stream_recv(&mut payload).await.is_none());
    assert!(ntex::util::stream_recv(&mut payload).await.is_none());
}

/// STATEMENT: An early error response is readable even when the peer closes with an unread request body.
///
/// HOW:
/// - POST a 64 MiB body to a raw server that sends a 413 response before draining the request and closes shortly afterward.
/// - Assert `PAYLOAD_TOO_LARGE` and the complete `too big!` response body.
#[ntex::test]
async fn client_early_response() {
    let addr = ntex::server::TestServer::unused_addr();
    let lst = std::net::TcpListener::bind(addr).unwrap();
    std::thread::spawn(move || {
        for mut stream in lst.incoming().flatten() {
            let mut b = [0; 1024];
            let _ = stream.read(&mut b);
            let _ = stream
                .write_all(b"HTTP/1.1 413 Payload Too Large\r\ncontent-length: 8\r\n\r\ntoo big!");
            std::thread::sleep(std::time::Duration::from_millis(200));
            // close with unread request body
            drop(stream);
        }
    });

    let body = Bytes::from(vec![b'x'; 64 * 1024 * 1024]);
    let response = Client::builder()
        .build(ClientConfig::new().set_response_timeout(Seconds(30)))
        .post(format!("http://{addr}/").as_str())
        .send_body(body)
        .await
        .unwrap();
    assert_eq!(response.status(), ntex::http::StatusCode::PAYLOAD_TOO_LARGE);
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"too big!"));
}

/// STATEMENT: Premature EOF in a fixed-length HTTP/1.0 body is an error.
///
/// HOW:
/// - Serve `welcome!` while advertising a 20-byte content length, then close.
/// - Assert the response head succeeds but reading its body fails.
#[ntex::test]
async fn client_truncated_body_http10() {
    let addr = raw_server(b"HTTP/1.0 200 OK\r\ncontent-length: 20\r\n\r\nwelcome!");

    let response = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(response.body().await.is_err());
}

/// STATEMENT: `basic_auth` sends the expected Base64 authorization value.
///
/// HOW:
/// - Serve success only for the expected Basic header encoding `username:password`.
/// - Send the credentials through `basic_auth` and assert success.
#[ntex::test]
async fn client_basic_auth() {
    let srv = test::server(async |_| {
        App::new().route(
            "/",
            web::to(async move |req: HttpRequest| {
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

/// STATEMENT: `bearer_auth` produces a request accepted by the handler's authorization check.
///
/// HOW:
/// - Serve a handler that compares the authorization header with its expected value.
/// - Send a bearer-authenticated request and assert a successful status.
#[ntex::test]
async fn client_bearer_auth() {
    let srv = test::server(async |_| {
        App::new().route(
            "/",
            web::to(async move |req: HttpRequest| {
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

/// STATEMENT: Client middleware surrounds the service call and leaves the response readable.
///
/// HOW:
/// - Install `fn_layer` recording the URI before and after the inner call; check the initial service request has no headers or address.
/// - Assert client readiness, success, the full `STR` body, and the ordered before/after records.
#[ntex::test]
async fn middleware() {
    let srv = test::server(async |_| {
        App::new().service(web::resource("/").route(web::to(async || HttpResponse::Ok().body(STR))))
    });

    let data = Rc::new(RefCell::new(Vec::new()));
    let data2 = data.clone();

    // client request
    let client = Client::builder()
        .middleware(fn_layer(
            async move |mut req: client::ServiceRequest, svc| {
                assert!(req.headers().is_empty());
                assert!(req.address().is_none());
                let s = req.head().uri.to_string();
                data2.borrow_mut().push(format!("1 -- {}", s));
                let result = svc.call(req).await;
                data2.borrow_mut().push(format!("2 -- {}", s));
                result
            },
        ))
        .build(SharedCfg::default());

    assert!(client.ready().await.is_ok());

    let request = client.get(srv.url("/")).header("x-test", "111").send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let host = srv.url("/");
    assert_eq!(
        &data.borrow()[..],
        &[format!("1 -- {}", host), format!("2 -- {}", host)]
    );
}

/// STATEMENT: The low-level HTTP test client reads GET and POST responses.
///
/// HOW:
/// - Serve `STR` through `HttpService`, then send GET, GET with a custom header, and POST.
/// - Assert all statuses succeed and the latter two bodies equal `STR`.
#[ntex::test]
async fn test_h1_v2() {
    let srv = test_server(async move |_| {
        HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    let request = srv.request(Method::GET, "/").header("x-test", "111").send();
    let response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    let response = srv.request(Method::POST, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: The generated `Host` header retains the server's nondefault port.
///
/// HOW:
/// - Echo the request's `HOST` header from a local route.
/// - Send an absolute-URL GET and assert the body is `localhost` followed by the server port.
#[ntex::test]
async fn untruncated_host_header() {
    let srv = test::server(async |_| {
        App::new().route(
            "/host",
            web::get().to(async move |req: HttpRequest| {
                req.headers()
                    .get(ntex::http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("<missing>")
                    .to_string()
            }),
        )
    });

    let port = srv.addr().port();
    let resp = srv
        .request(ntex::http::Method::GET, srv.url("/host"))
        .send()
        .await
        .unwrap();
    let body = resp.body().await.unwrap();
    let received = String::from_utf8(body.to_vec()).unwrap();

    assert_eq!(received, format!("localhost:{port}"));
}

/// STATEMENT: The QUERY method routes and transports its request body.
///
/// HOW:
/// - Register a `web::query` route that collects and echoes its payload.
/// - Send `SuperBody` with the client's `query` method and assert the response body matches.
#[ntex::test]
async fn test_query_method() {
    use ntex::util::BytesMut;
    let srv = test::server(async |_| {
        App::new().route(
            "/query",
            web::query().to(async move |mut payload: web::types::Payload| {
                let mut bytes = BytesMut::new();
                while let Some(item) = ntex::util::stream_recv(&mut payload).await {
                    bytes.extend_from_slice(&item.unwrap());
                }
                bytes
            }),
        )
    });

    let test_body = Bytes::from_static(b"SuperBody");

    let resp = srv
        .query("/query")
        .send_body(test_body.clone())
        .await
        .unwrap();
    let body = resp.body().await.unwrap();

    assert_eq!(test_body, body);
}

/// STATEMENT: Malformed chunk framing surfaces as a payload decode error.
///
/// HOW:
/// - Serve a successful response head followed by an invalid chunk terminator through `raw_server`.
/// - Assert the head succeeds and reading the body yields `PayloadError::Decode`.
#[ntex::test]
async fn client_invalid_chunked_body() {
    let addr =
        raw_server(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n4\r\ndataX\r\n0\r\n\r\n");

    let response = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let err = response.body().await.unwrap_err().into_error();
    assert!(
        matches!(*err, ntex::http::error::PayloadError::Decode(_)),
        "{err:?}"
    );
}

/// STATEMENT: Closure before any response head produces a disconnected connection error.
///
/// HOW:
/// - Use `raw_server` with an empty response and issue GET.
/// - Assert `ClientError::Connect` contains `ConnectError::Disconnected`.
#[ntex::test]
async fn client_no_response() {
    let addr = raw_server(b"");

    let err = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap_err();
    assert!(matches!(
        err.into_error(),
        ClientError::Connect(client::error::ConnectError::Disconnected(_))
    ));
}

/// STATEMENT: An invalid status line produces a response parsing error.
///
/// HOW:
/// - Serve an HTTP/1.1 status containing `2x0` through `raw_server`.
/// - Assert sending the request fails with `ClientError::Response`.
#[ntex::test]
async fn client_invalid_response_head() {
    let addr = raw_server(b"HTTP/1.1 2x0 OK\r\n\r\n");

    let err = Client::new()
        .get(format!("http://{addr}/").as_str())
        .send()
        .await
        .unwrap_err();
    assert!(matches!(err.into_error(), ClientError::Response(_)));
}

/// STATEMENT: Bodiless responses ignore an advertised `Content-Length` larger
/// than the client body limit.
///
/// HOW:
/// - Serve a gzip-encoded `HEAD` response, a `204` and a `304`, each
///   advertising `content-length: 1000000` (above the 256 KiB default limit)
///   via `raw_server`.
/// - Assert each keeps its `Content-Length` header (decompression does not
///   strip it) and `body()` returns empty bytes instead of `Overflow`.
#[ntex::test]
async fn client_bodiless_response_ignores_content_length_limit() {
    let head =
        raw_server(b"HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: 1000000\r\n\r\n");
    let no_content = raw_server(b"HTTP/1.1 204 No Content\r\ncontent-length: 1000000\r\n\r\n");
    let not_modified = raw_server(b"HTTP/1.1 304 Not Modified\r\ncontent-length: 1000000\r\n\r\n");

    let client = Client::new();
    let requests = [
        client.head(format!("http://{head}/").as_str()),
        client.get(format!("http://{no_content}/").as_str()),
        client.get(format!("http://{not_modified}/").as_str()),
    ];
    for req in requests {
        let response = req.send().await.unwrap();
        assert_eq!(
            response.headers().get(header::CONTENT_LENGTH).unwrap(),
            "1000000"
        );
        assert_eq!(response.body().await.unwrap(), Bytes::new());
    }
}

/// STATEMENT: Pooled connections are keyed by the `address()` override, so a
/// request for the same authority with another address reaches that server.
///
/// HOW:
/// - Start two keep-alive servers that answer `a` and `b`.
/// - Send requests for one URL authority to `a`, then `b`, then `a` again with
///   `address()`, reading each body so the connection returns to the pool.
/// - Assert each body names the addressed server and server `a` accepted only
///   one connection, so its pooled connection was reused.
#[ntex::test]
async fn client_pool_respects_address_override() {
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted2 = accepted.clone();
    let srv_a = test_server(async move |_| {
        let accepted = accepted2.clone();
        service(async move |io| {
            accepted.fetch_add(1, Ordering::Relaxed);
            Ok(io)
        })
        .and_then(HttpService::new(
            App::new().service(web::resource("/").route(web::to(async || "a"))),
        ))
    });
    let srv_b = test_server(async |_| {
        HttpService::new(App::new().service(web::resource("/").route(web::to(async || "b"))))
    });

    let client = Client::new();
    for (addr, expected) in [
        (srv_a.addr(), "a"),
        (srv_b.addr(), "b"),
        (srv_a.addr(), "a"),
    ] {
        let response = client
            .get("http://pool.example:80/")
            .address(addr)
            .send()
            .await
            .unwrap();
        assert_eq!(response.body().await.unwrap(), Bytes::from(expected));
    }
    assert_eq!(accepted.load(Ordering::Relaxed), 1);
}
