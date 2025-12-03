use std::task::{Context, Poll, ready};
use std::{future::Future, io, io::Read, io::Write, pin::Pin};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::{GzEncoder, ZlibDecoder, ZlibEncoder};
use rand::{Rng, distr::Alphanumeric};
use thiserror::Error;

use ntex::http::header::{
    ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ContentEncoding,
    TRANSFER_ENCODING,
};
use ntex::http::{ConnectionType, HttpServiceConfig, Method, StatusCode, body::Body};
use ntex::time::{Millis, Seconds, Sleep, sleep};
use ntex::util::{Bytes, Ready, Stream};
use ntex::{SharedCfg, client, io::IoConfig};

use ntex::web::{self, middleware::Compress, test};
use ntex::web::{App, BodyEncoding, HttpRequest, HttpResponse, WebResponseError};

#[cfg(feature = "rustls")]
mod rustls_utils;

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

struct TestBody {
    data: Bytes,
    chunk_size: usize,
    delay: Sleep,
}

impl TestBody {
    fn new(data: Bytes, chunk_size: usize) -> Self {
        TestBody {
            data,
            chunk_size,
            delay: sleep(Millis(10)),
        }
    }
}

impl Stream for TestBody {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        ready!(Pin::new(&mut self.delay).poll(cx));

        self.delay = sleep(Millis(10));
        let chunk_size = std::cmp::min(self.chunk_size, self.data.len());
        let chunk = self.data.split_to(chunk_size);
        if chunk.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(chunk)))
        }
    }
}

#[ntex::test]
async fn test_body() {
    let srv = test::server(async || {
        App::new().service(
            web::resource("/").route(web::to(|| async { HttpResponse::Ok().body(STR) })),
        )
    })
    .await;

    let mut response = srv.get("/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_body_gzip() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(
                web::resource("/")
                    .route(web::to(|| async { HttpResponse::Ok().body(STR) })),
            )
    })
    .await;

    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_body_gzip2() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(web::resource("/").route(web::to(|| async {
                HttpResponse::Ok().body(STR).into_body::<Body>()
            })))
    })
    .await;

    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_body_encoding_override() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(web::resource("/").route(web::to(|| async {
                HttpResponse::Ok()
                    .encoding(ContentEncoding::Deflate)
                    .body(STR)
            })))
            .service(web::resource("/raw").route(web::to(|| async {
                let body = Body::Bytes(STR.into());
                let mut response = HttpResponse::with_body(StatusCode::OK, body);

                response.encoding(ContentEncoding::Deflate);

                response
            })))
    })
    .await;

    // Builder
    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "deflate")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = ZlibDecoder::new(Vec::new());
    e.write_all(bytes.as_ref()).unwrap();
    let dec = e.finish().unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));

    // Raw Response
    let mut response = srv
        .request(Method::GET, srv.url("/raw"))
        .no_decompress()
        .header(ACCEPT_ENCODING, "deflate")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = ZlibDecoder::new(Vec::new());
    e.write_all(bytes.as_ref()).unwrap();
    let dec = e.finish().unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_body_gzip_large() {
    let data = STR.repeat(10);
    let srv_data = data.clone();

    let srv = test::server_with(test::config().h1(), async move || {
        let data = srv_data.clone();
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(web::resource("/").route(web::to(move || {
                Ready::Ok::<_, io::Error>(HttpResponse::Ok().body(data.clone()))
            })))
    })
    .await;

    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from(data));
}

#[ntex::test]
async fn test_body_gzip_large_random() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(70_000)
        .map(char::from)
        .collect::<String>();
    let srv_data = data.clone();

    let srv = test::server_with(test::config().h1(), async move || {
        let data = srv_data.clone();
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(web::resource("/").route(web::to(move || {
                Ready::Ok::<_, io::Error>(HttpResponse::Ok().body(data.clone()))
            })))
    })
    .await;

    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(dec.len(), data.len());
    assert_eq!(Bytes::from(dec), Bytes::from(data));
}

#[ntex::test]
async fn test_body_chunked_implicit() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::new(ContentEncoding::Gzip))
            .service(web::resource("/").route(web::get().to(move || async {
                HttpResponse::Ok()
                    .streaming(TestBody::new(Bytes::from_static(STR.as_ref()), 24))
            })))
    })
    .await;

    let mut response = srv
        .get("/")
        .no_decompress()
        .header(ACCEPT_ENCODING, "gzip")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response.headers().get(TRANSFER_ENCODING).unwrap(),
        &b"chunked"[..]
    );

    // read response
    let bytes = response.body().await.unwrap();

    // decode
    let mut e = GzDecoder::new(&bytes[..]);
    let mut dec = Vec::new();
    e.read_to_end(&mut dec).unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_head_binary() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(
            web::resource("/")
                .route(web::head().to(move || async {
                    HttpResponse::Ok().content_length(100).body(STR)
                })),
        )
    })
    .await;

    let mut response = srv.head("/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }

    // read response
    let bytes = response.body().await.unwrap();
    assert!(bytes.is_empty());
}

#[ntex::test]
async fn test_no_chunking() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move || async {
            HttpResponse::Ok()
                .no_chunking()
                .content_length(STR.len() as u64)
                .streaming(TestBody::new(Bytes::from_static(STR.as_ref()), 24))
        })))
    })
    .await;

    let mut response = srv.get("/").send().await.unwrap();
    assert!(response.status().is_success());
    assert!(!response.headers().contains_key(TRANSFER_ENCODING));

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_body_deflate() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::new(ContentEncoding::Deflate))
            .service(
                web::resource("/")
                    .route(web::to(move || async { HttpResponse::Ok().body(STR) })),
            )
    })
    .await;

    // client request
    let mut response = srv
        .get("/")
        .header(ACCEPT_ENCODING, "deflate")
        .no_decompress()
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();

    let mut e = ZlibDecoder::new(Vec::new());
    e.write_all(bytes.as_ref()).unwrap();
    let dec = e.finish().unwrap();
    assert_eq!(Bytes::from(dec), Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_encoding() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .wrap(Compress::default())
            .service(web::resource("/").route(web::to(move |body: Bytes| async {
                HttpResponse::Ok().body(body)
            })))
    })
    .await;

    // client request
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(STR.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "gzip")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_gzip_encoding() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    // client request
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(STR.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "gzip")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_gzip_encoding_large() {
    let data = STR.repeat(10);
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    // client request
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "gzip")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from(data));
}

#[ntex::test]
async fn test_reading_gzip_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(60_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    // client request
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "gzip")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(bytes, Bytes::from(data));
}

#[ntex::test]
async fn test_reading_deflate_encoding() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(STR.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "deflate")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_reading_deflate_encoding_large() {
    let data = STR.repeat(10);
    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "deflate")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from(data));
}

#[ntex::test]
async fn test_reading_deflate_encoding_large_random() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(160_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(test::config().h1(), async || {
        App::new().service(web::resource("/").route(web::to(move |body: Bytes| async {
            HttpResponse::Ok().body(body)
        })))
    })
    .await;

    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let request = srv
        .post("/")
        .header(CONTENT_ENCODING, "deflate")
        .send_body(enc.clone());
    let mut response = request.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(bytes, Bytes::from(data));
}

#[cfg(all(feature = "rustls", feature = "openssl"))]
#[ntex::test]
async fn test_reading_deflate_encoding_large_random_rustls() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(160_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(
        test::config().rustls(rustls_utils::tls_acceptor()),
        async || {
            App::new().service(web::resource("/").route(web::to(|bytes: Bytes| async {
                HttpResponse::Ok()
                    .encoding(ContentEncoding::Identity)
                    .body(bytes)
            })))
        },
    )
    .await;

    // encode data
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let req = srv
        .post("/")
        .timeout(Millis(30_000))
        .header(CONTENT_ENCODING, "deflate")
        .send_stream(TestBody::new(Bytes::from(enc), 1024));

    let mut response = req.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(bytes, Bytes::from(data));
}

#[cfg(all(feature = "rustls", feature = "openssl"))]
#[ntex::test]
async fn test_reading_deflate_encoding_large_random_rustls_h1() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(160_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(
        test::config().rustls(rustls_utils::tls_acceptor()).h1(),
        async || {
            App::new().service(web::resource("/").route(web::to(|bytes: Bytes| async {
                HttpResponse::Ok()
                    .encoding(ContentEncoding::Identity)
                    .body(bytes)
            })))
        },
    )
    .await;

    // encode data
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let req = srv
        .post("/")
        .timeout(Millis(30_000))
        .header(CONTENT_ENCODING, "deflate")
        .send_stream(TestBody::new(Bytes::from(enc), 1024));

    let mut response = req.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(bytes, Bytes::from(data));
}

#[cfg(all(feature = "rustls", feature = "openssl"))]
#[ntex::test]
async fn test_reading_deflate_encoding_large_random_rustls_h2() {
    let data = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(160_000)
        .map(char::from)
        .collect::<String>();

    let srv = test::server_with(
        test::config().rustls(rustls_utils::tls_acceptor()).h2(),
        async || {
            App::new().service(web::resource("/").route(web::to(|bytes: Bytes| async {
                HttpResponse::Ok()
                    .encoding(ContentEncoding::Identity)
                    .body(bytes)
            })))
        },
    )
    .await;

    // encode data
    let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
    e.write_all(data.as_ref()).unwrap();
    let enc = e.finish().unwrap();

    // client request
    let req = srv
        .post("/")
        .timeout(Millis(30_000))
        .header(CONTENT_ENCODING, "deflate")
        .send_stream(TestBody::new(Bytes::from(enc), 1024));

    let mut response = req.await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes.len(), data.len());
    assert_eq!(bytes, Bytes::from(data));
}

#[ntex::test]
async fn test_server_cookies() {
    use ntex::http::HttpMessage;
    use ntex::http::header::SET_COOKIE;

    let srv = test::server(async || {
        App::new().service(web::resource("/").to(|| async {
            HttpResponse::Ok()
                .cookie(coo_kie::Cookie::build(("first", "first_value")).http_only(true))
                .cookie(coo_kie::Cookie::new("second", "first_value"))
                .cookie(coo_kie::Cookie::new("second", "second_value"))
                .finish()
        }))
    })
    .await;

    let first_cookie = coo_kie::Cookie::build(("first", "first_value")).http_only(true);
    let second_cookie = coo_kie::Cookie::new("second", "second_value");

    let response = srv.get("/").send().await.unwrap();
    assert!(response.status().is_success());

    let cookies = response.cookies().expect("To have cookies");
    assert_eq!(cookies.len(), 2);
    if cookies[0] == first_cookie {
        assert_eq!(cookies[1], second_cookie);
    } else {
        assert_eq!(cookies[0], second_cookie);
        assert_eq!(cookies[1], first_cookie);
    }

    let first_cookie = first_cookie.to_string();
    let second_cookie = second_cookie.to_string();
    // Check that we have exactly two instances of raw cookie headers
    let cookies = response
        .headers()
        .get_all(SET_COOKIE)
        .map(|header| header.to_str().expect("To str").to_string())
        .collect::<Vec<_>>();
    assert_eq!(cookies.len(), 2);
    if cookies[0] == first_cookie {
        assert_eq!(cookies[1], second_cookie);
    } else {
        assert_eq!(cookies[0], second_cookie);
        assert_eq!(cookies[1], first_cookie);
    }
}

#[ntex::test]
async fn test_slow_request() {
    use std::net;

    let srv = test::server_with(test::config().client_timeout(Seconds(1)), async || {
        App::new()
            .service(web::resource("/").route(web::to(|| async { HttpResponse::Ok() })))
    })
    .await;

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"));

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"));
}

#[ntex::test]
async fn test_custom_error() {
    #[derive(Error, Debug)]
    #[error("TestError")]
    struct TestError;

    #[derive(Error, Debug)]
    #[error("JsonContainer({0})")]
    struct JsonContainer(Box<dyn WebResponseError<JsonRenderer>>);

    impl ntex::web::ErrorContainer for JsonContainer {
        fn error_response(&self, req: &HttpRequest) -> HttpResponse {
            self.0.error_response(req)
        }
    }

    impl ntex::http::ResponseError for JsonContainer {}

    impl From<TestError> for JsonContainer {
        fn from(e: TestError) -> JsonContainer {
            JsonContainer(Box::new(e))
        }
    }

    struct JsonRenderer;

    impl ntex::web::error::ErrorRenderer for JsonRenderer {
        type Container = JsonContainer;
    }

    impl WebResponseError<JsonRenderer> for TestError {
        fn error_response(&self, _: &HttpRequest) -> HttpResponse {
            HttpResponse::BadRequest()
                .header(CONTENT_TYPE, "application/json")
                .body("Error")
        }
    }

    async fn test() -> Result<HttpResponse, TestError> {
        Ok(HttpResponse::Ok().body(STR))
    }

    async fn test_err() -> Result<HttpResponse, TestError> {
        Err(TestError)
    }

    let srv = test::server_with(test::config().h1(), async || {
        App::with(JsonRenderer)
            .service(web::resource("/").route(web::get().to(test)))
            .service(web::resource("/err").route(web::get().to(test_err)))
    })
    .await;

    let mut response = srv.get("/").send().await.unwrap();
    assert!(response.status().is_success());

    let len = response.headers().get(CONTENT_LENGTH).unwrap();
    assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());

    // read response
    let bytes = response.body().await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));

    // err
    let response = srv.get("/err").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let tp = response.headers().get(CONTENT_TYPE).unwrap();
    assert_eq!("application/json", tp.to_str().unwrap());
}

#[ntex::test]
async fn test_web_server() {
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let sys = ntex::rt::System::new("test-server");
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = tcp.local_addr().unwrap();
        tx.send((sys.system(), local_addr)).unwrap();

        let _ = sys.block_on(async move {
            web::server(async || {
                App::new().service(
                    web::resource("/")
                        .route(web::to(|| async { HttpResponse::Ok().body(STR) })),
                )
            })
            .config(
                SharedCfg::new("TEST")
                    .add(IoConfig::new().set_disconnect_timeout(Seconds(1)))
                    .add(
                        HttpServiceConfig::new()
                            .set_headers_read_rate(Seconds(1), Seconds(5), 128)
                            .set_payload_read_rate(Seconds(1), Seconds(5), 128),
                    ),
            )
            .listen(tcp)
            .unwrap()
            .run()
            .await
        });
    });
    let (system, addr) = rx.recv().unwrap();

    let client = client::Client::build()
        .response_timeout(Seconds(30))
        .finish(SharedCfg::default())
        .await
        .unwrap();

    let response = client
        .request(Method::GET, format!("http://{addr:?}/"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    system.stop();
}

/// Websocket connection, no ws handler and response contains payload
#[ntex::test]
async fn web_no_ws_with_response_payload() {
    let srv = test::server_with(test::config().h1(), async || {
        App::new()
            .service(web::resource("/").route(web::get().to(move || async {
                HttpResponse::Ok()
                    .streaming(TestBody::new(Bytes::from_static(STR.as_ref()), 24))
            })))
            .service(
                web::resource("/f")
                    .route(web::get().to(move || async { HttpResponse::Ok().body(STR) })),
            )
    })
    .await;

    let client = client::Client::build()
        .response_timeout(Seconds(30))
        .finish(SharedCfg::default())
        .await
        .unwrap();
    let mut response = client
        .request(Method::GET, format!("http://{:?}/f", srv.addr()))
        .header("sec-websocket-version", "13")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "ld75/p3D5ju5UhWsNMcJHA==")
        .set_connection_type(ConnectionType::Upgrade)
        .send()
        .await
        .unwrap();
    let body = response.body().await.unwrap();
    assert_eq!(body, STR);

    let mut response = client
        .request(Method::GET, format!("http://{:?}/", srv.addr()))
        .header("sec-websocket-version", "13")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "ld75/p3D5ju5UhWsNMcJHA==")
        .set_connection_type(ConnectionType::Upgrade)
        .send()
        .await
        .unwrap();
    let body = response.body().await.unwrap();
    assert_eq!(body, STR);
}
