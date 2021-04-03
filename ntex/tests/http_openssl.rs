#![cfg(feature = "openssl")]
use std::io;

use bytes::{Bytes, BytesMut};
use futures::future::{err, ok, ready};
use futures::stream::{once, Stream, StreamExt};
use open_ssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

use ntex::http::error::PayloadError;
use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::test::server as test_server;
use ntex::http::{body, HttpService, Method, Request, Response, StatusCode, Version};
use ntex::service::{fn_service, ServiceFactory};
use ntex::web::error::InternalError;

async fn load_body<S>(stream: S) -> Result<BytesMut, PayloadError>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    let body = stream
        .map(|res| if let Ok(chunk) = res { chunk } else { panic!() })
        .fold(BytesMut::new(), move |mut body, chunk| {
            body.extend_from_slice(&chunk);
            ready(body)
        })
        .await;

    Ok(body)
}

fn ssl_acceptor() -> SslAcceptor {
    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    builder.set_alpn_select_callback(|_, protos| {
        const H2: &[u8] = b"\x02h2";
        const H11: &[u8] = b"\x08http/1.1";
        if protos.windows(3).any(|window| window == H2) {
            Ok(b"h2")
        } else if protos.windows(9).any(|window| window == H11) {
            Ok(b"http/1.1")
        } else {
            Err(AlpnError::NOACK)
        }
    });
    builder
        .set_alpn_protos(b"\x08http/1.1\x02h2")
        .expect("Cannot contrust SslAcceptor");

    builder.build()
}

#[ntex::test]
async fn test_h2() -> io::Result<()> {
    let srv = test_server(move || {
        HttpService::build()
            .h2(|_| ok::<_, io::Error>(Response::Ok().finish()))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

#[ntex::test]
async fn test_h1() -> io::Result<()> {
    let srv = test_server(move || {
        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        builder
            .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
            .unwrap();
        builder
            .set_certificate_chain_file("./tests/cert.pem")
            .unwrap();

        HttpService::build()
            .h1(|_| ok::<_, io::Error>(Response::Ok().finish()))
            .openssl(builder.build())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

#[ntex::test]
async fn test_h2_1() -> io::Result<()> {
    let srv = test_server(move || {
        HttpService::build()
            .finish(|req: Request| {
                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_2);
                ok::<_, io::Error>(Response::Ok().finish())
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

#[ntex::test]
async fn test_h2_body() -> io::Result<()> {
    let data = "HELLOWORLD".to_owned().repeat(64 * 1024);
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|mut req: Request| async move {
                let body = load_body(req.take_payload())
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                Ok::<_, io::Error>(Response::Ok().body(body))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv
        .srequest(Method::GET, "/")
        .send_body(data.clone())
        .await
        .unwrap();
    assert!(response.status().is_success());

    let body = srv.load_body(response).await.unwrap();
    assert_eq!(&body, data.as_bytes());
    Ok(())
}

#[ntex::test]
async fn test_h2_content_length() {
    let srv = test_server(move || {
        HttpService::build()
            .h2(|req: Request| {
                let indx: usize = req.uri().path()[1..].parse().unwrap();
                let statuses = [
                    StatusCode::NO_CONTENT,
                    // h2 lib does not accept hangs on this statuses
                    //StatusCode::CONTINUE,
                    //StatusCode::SWITCHING_PROTOCOLS,
                    //StatusCode::PROCESSING,
                    StatusCode::OK,
                    StatusCode::NOT_FOUND,
                ];
                ok::<_, io::Error>(Response::new(statuses[indx]))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let header = HeaderName::from_static("content-length");
    let value = HeaderValue::from_static("0");

    {
        for i in 0..1 {
            let req = srv.srequest(Method::GET, format!("/{}", i)).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);

            let req = srv.srequest(Method::HEAD, format!("/{}", i)).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);
        }

        for i in 1..3 {
            let req = srv.srequest(Method::GET, format!("/{}", i)).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), Some(&value));
        }
    }
}

#[ntex::test]
async fn test_h2_headers() {
    let data = STR.repeat(10);
    let data2 = data.clone();

    let mut srv = test_server(move || {
        let data = data.clone();
        HttpService::build().h2(move |_| {
            let mut builder = Response::Ok();
            for idx in 0..90 {
                builder.header(
                    format!("X-TEST-{}", idx).as_str(),
                    "TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST \
                        TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST TEST ",
                );
            }
            ok::<_, io::Error>(builder.body(data.clone()))
        })
            .openssl(ssl_acceptor())
                    .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from(data2));
}

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
async fn test_h2_body2() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| ok::<_, io::Error>(Response::Ok().body(STR)))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h2_head_empty() {
    let mut srv = test_server(move || {
        HttpService::build()
            .finish(|_| ok::<_, io::Error>(Response::Ok().body(STR)))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::HEAD, "/").send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(response.version(), Version::HTTP_2);

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert!(bytes.is_empty());
}

#[ntex::test]
async fn test_h2_head_binary() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| {
                ok::<_, io::Error>(
                    Response::Ok().content_length(STR.len() as u64).body(STR),
                )
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::HEAD, "/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert!(bytes.is_empty());
}

#[ntex::test]
async fn test_h2_head_binary2() {
    let srv = test_server(move || {
        HttpService::build()
            .h2(|_| ok::<_, io::Error>(Response::Ok().body(STR)))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::HEAD, "/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }
}

#[ntex::test]
async fn test_h2_body_length() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| {
                let body = once(ok(Bytes::from_static(STR.as_ref())));
                ok::<_, io::Error>(
                    Response::Ok().body(body::SizedStream::new(STR.len() as u64, body)),
                )
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h2_body_chunked_explicit() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| {
                let body = once(ok::<_, io::Error>(Bytes::from_static(STR.as_ref())));
                ok::<_, io::Error>(
                    Response::Ok()
                        .header(header::TRANSFER_ENCODING, "chunked")
                        .streaming(body),
                )
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));

    // read response
    let bytes = srv.load_body(response).await.unwrap();

    // decode
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h2_response_http_error_handling() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(fn_service(|_| {
                let broken_header = Bytes::from_static(b"\0\0\0");
                ok::<_, io::Error>(
                    Response::Ok()
                        .header(header::CONTENT_TYPE, &broken_header[..])
                        .body(STR),
                )
            }))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"failed to parse header value"));
}

#[ntex::test]
async fn test_h2_service_error() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| {
                err::<Response, _>(InternalError::default(
                    "error",
                    StatusCode::BAD_REQUEST,
                ))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"error"));
}

#[ntex::test]
async fn test_h2_on_connect() {
    let srv = test_server(move || {
        HttpService::build()
            .on_connect(|_| 10usize)
            .h2(|req: Request| {
                assert!(req.extensions().contains::<usize>());
                ok::<_, io::Error>(Response::Ok().finish())
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_ssl_handshake_timeout() {
    use std::io::Read;

    let srv = test_server(move || {
        HttpService::build()
            .ssl_handshake_timeout(1)
            .h2(|_| ok::<_, io::Error>(Response::Ok().finish()))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let mut stream = std::net::TcpStream::connect(srv.addr()).unwrap();
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.is_empty());
}
