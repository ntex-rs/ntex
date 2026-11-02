#![cfg(feature = "openssl")]
use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{future::ready, io};

use futures_util::stream::{Stream, StreamExt, once};
use tls_openssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

use ntex::codec::BytesCodec;
use ntex::http::error::PayloadError;
use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::test::{self, server as test_server};
use ntex::http::{HttpService, Method, Request, Response, StatusCode, Version, body, h1, openssl};
use ntex::service::cfg::SharedCfg;
use ntex::time::{Millis, Seconds, sleep, timeout};
use ntex::util::{Bytes, BytesMut, Either, select};
use ntex::ws::{self, handshake_response};
use ntex::{channel::oneshot, client, rt, web::error::InternalError};
use ntex_tls::TlsConfig;

async fn load_body<S>(stream: S) -> Result<BytesMut, PayloadError>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    let body = stream
        .map(|res| if let Ok(chunk) = res { chunk } else { panic!() })
        .fold(BytesMut::new(), async move |mut body, chunk| {
            body.extend_from_slice(&chunk);
            body
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

/// STATEMENT: An HTTP/2-only service accepts secure GET requests through OpenSSL.
///
/// HOW:
/// - Start a TLS-wrapped `HttpService::h2` and send a secure GET.
/// - Assert the response status is successful.
#[ntex::test]
async fn test_h2() -> io::Result<()> {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

/// STATEMENT: HTTP/2 expect control can acknowledge or reject a request through OpenSSL.
///
/// HOW:
/// - Count control messages and echo a POST carrying `EXPECT: 100-continue`; assert HTTP/2, success, the echoed body, and a control invocation.
/// - Send another expect request to `/reject` and assert `EXPECTATION_FAILED`.
#[ntex::test]
async fn test_h2_control() -> io::Result<()> {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let srv = test_server(async move |_| {
        let num = num2.clone();
        openssl(
            ssl_acceptor(),
            HttpService::new(async |mut req: Request| {
                let body = load_body(req.take_payload()).await.unwrap();
                Ok::<_, io::Error>(Response::Ok().body(body.freeze()))
            })
            .h2_control(move |msg: ntex::http::h2::Control<_>| {
                num.fetch_add(1, Ordering::Relaxed);
                async move {
                    Ok::<_, io::Error>(match msg {
                        ntex::http::h2::Control::Expect(expect)
                            if expect.pseudo().path.as_deref() == Some("/reject") =>
                        {
                            expect.fail(StatusCode::EXPECTATION_FAILED, header::HeaderMap::new())
                        }
                        msg => msg.ack(),
                    })
                }
            }),
        )
    });

    let response = srv
        .srequest(Method::POST, "/")
        .header(header::EXPECT, "100-continue")
        .send_body("data")
        .await
        .unwrap();
    assert_eq!(response.version(), Version::HTTP_2);
    assert!(response.status().is_success());
    assert_eq!(response.body().await.unwrap(), "data");
    assert!(num.load(Ordering::Relaxed) >= 1);

    let response = srv
        .srequest(Method::POST, "/reject")
        .header(header::EXPECT, "100-continue")
        .send_body("data")
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::EXPECTATION_FAILED);
    Ok(())
}

/// STATEMENT: An HTTP/1-only service accepts secure GET requests through OpenSSL.
///
/// HOW:
/// - Start a TLS-wrapped `HttpService::h1` and send a secure GET.
/// - Assert the response status is successful.
#[ntex::test]
async fn test_h1() -> io::Result<()> {
    let srv = test_server(async move |_| {
        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        builder
            .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
            .unwrap();
        builder
            .set_certificate_chain_file("./tests/cert.pem")
            .unwrap();

        openssl(
            builder.build(),
            HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

/// STATEMENT: Pooled HTTP/2 request headers do not leak between requests through OpenSSL.
///
/// HOW:
/// - Repeatedly send headers `x-a` and `x-b`, then a separate request with only `x-c`.
/// - Assert HTTP/2 on the first response and each echoed header list contains only its own request's headers.
#[ntex::test]
async fn test_h2_request_headers_not_shared() -> io::Result<()> {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |req: Request| {
                let mut hdrs: Vec<_> = req
                    .headers()
                    .iter()
                    .filter(|(name, _)| name.as_str().starts_with("x-"))
                    .map(|(name, value)| format!("{name}={}", value.to_str().unwrap()))
                    .collect();
                hdrs.sort();
                Ok::<_, io::Error>(Response::Ok().body(hdrs.join(",")))
            }),
        )
    });

    // request header maps are reused, a request must not see headers of a previous one
    for _ in 0..3 {
        let response = srv
            .srequest(Method::GET, "/")
            .header("x-a", "1")
            .header("x-b", "2")
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), Version::HTTP_2);
        assert_eq!(response.body().await.unwrap(), "x-a=1,x-b=2");

        let response = srv
            .srequest(Method::GET, "/")
            .header("x-c", "3")
            .send()
            .await
            .unwrap();
        assert_eq!(response.body().await.unwrap(), "x-c=3");
    }
    Ok(())
}

/// STATEMENT: The combined HTTP service dispatches negotiated HTTP/2 requests with peer metadata through OpenSSL.
///
/// HOW:
/// - Start `HttpService::new` over TLS and send secure GET.
/// - Assert the handler sees a peer address and `Version::HTTP_2`, and the response succeeds.
#[ntex::test]
async fn test_h2_1() -> io::Result<()> {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::new(async |req: Request| {
                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_2);
                Ok::<_, io::Error>(Response::Ok().build())
            }),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    Ok(())
}

/// STATEMENT: A large HTTP/2 request payload survives an echo round trip through OpenSSL.
///
/// HOW:
/// - Send `HELLOWORLD` repeated 65,536 times to a handler collecting `Request::take_payload`.
/// - Assert success and a response body byte-for-byte equal to the sent data.
#[ntex::test]
async fn test_h2_body() -> io::Result<()> {
    let data = "HELLOWORLD".to_owned().repeat(64 * 1024);
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async move |mut req: Request| {
                let body = load_body(req.take_payload())
                    .await
                    .map_err(io::Error::other)?;
                Ok::<_, io::Error>(Response::Ok().body(body))
            }),
        )
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

/// STATEMENT: HTTP/2 content-length headers respect bodyless and empty-body statuses through OpenSSL.
///
/// HOW:
/// - Return 204, 200, or 404 based on the request path.
/// - Assert 204 GET and HEAD omit `content-length`, while empty 200 and 404 GET responses advertise `0`.
#[ntex::test]
async fn test_h2_content_length() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async move |req: Request| {
                let indx: usize = req.uri().path().as_str()[1..].parse().unwrap();
                let statuses = [
                    StatusCode::NO_CONTENT,
                    // h2 lib does not accept hangs on this statuses
                    //StatusCode::CONTINUE,
                    //StatusCode::SWITCHING_PROTOCOLS,
                    //StatusCode::PROCESSING,
                    StatusCode::OK,
                    StatusCode::NOT_FOUND,
                ];
                Ok::<_, io::Error>(Response::new(statuses[indx]))
            }),
        )
    });

    let header = HeaderName::from_static("content-length");
    let value = HeaderValue::from_static("0");

    {
        for i in 0..1 {
            let req = srv.srequest(Method::GET, format!("/{i}")).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);

            let req = srv.srequest(Method::HEAD, format!("/{i}")).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), None);
        }

        for i in 1..3 {
            let req = srv.srequest(Method::GET, format!("/{i}")).send();
            let response = req.await.unwrap();
            assert_eq!(response.headers().get(&header), Some(&value));
        }
    }
}

/// STATEMENT: A large HTTP/2 response header block does not corrupt its body through OpenSSL.
///
/// HOW:
/// - Return 90 long custom headers alongside `STR.repeat(10)`.
/// - Assert success and the complete response body matches the original data.
#[ntex::test]
async fn test_h2_headers() {
    let data = STR.repeat(10);
    let data2 = data.clone();

    let srv = test_server(async move |_| {
        let data = data.clone();
        openssl(
            ssl_acceptor(),
            HttpService::h2(async move |_| {
                let mut builder = Response::Ok();
                for idx in 0..90 {
                    builder.header(
                    format!("X-TEST-{idx}").as_str(),
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
                Ok::<_, io::Error>(builder.body(data.clone()))
            }),
        )
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

/// STATEMENT: An HTTP/2 fixed response body is delivered intact through OpenSSL.
///
/// HOW:
/// - Serve `STR` from an HTTP/2-only service and send secure GET.
/// - Assert success and the collected body equals `STR`.
#[ntex::test]
async fn test_h2_body2() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| Ok::<_, io::Error>(Response::Ok().body(STR))),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: HTTP/2 HEAD advertises the response size but sends no payload through OpenSSL.
///
/// HOW:
/// - Serve `STR` and send HEAD.
/// - Assert success, `Version::HTTP_2`, `CONTENT_LENGTH` equal to `STR.len()`, and an empty received body.
#[ntex::test]
async fn test_h2_head_empty() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().body(STR))),
        )
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

/// STATEMENT: HTTP/2 HEAD preserves explicit body length without sending the payload through OpenSSL.
///
/// HOW:
/// - Serve `STR` with explicit `content_length` and send HEAD.
/// - Assert success, the expected `CONTENT_LENGTH`, and an empty received body.
#[ntex::test]
async fn test_h2_head_binary() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| {
                Ok::<_, io::Error>(Response::Ok().content_length(STR.len() as u64).body(STR))
            }),
        )
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

/// STATEMENT: HTTP/2 HEAD derives content length from a fixed body through OpenSSL.
///
/// HOW:
/// - Serve `STR` without explicitly setting its length and send HEAD.
/// - Assert success and `CONTENT_LENGTH` equal to `STR.len()`.
#[ntex::test]
async fn test_h2_head_binary2() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| Ok::<_, io::Error>(Response::Ok().body(STR))),
        )
    });

    let response = srv.srequest(Method::HEAD, "/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }
}

/// STATEMENT: A sized streaming HTTP/2 response delivers its complete payload through OpenSSL.
///
/// HOW:
/// - Wrap a one-chunk `STR` stream in `SizedStream` using its known length.
/// - Send secure GET, assert success, and compare the collected body with `STR`.
#[ntex::test]
async fn test_h2_body_length() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| {
                let body = once(ready(Ok(Bytes::from_static(STR.as_ref()))));
                Ok::<_, io::Error>(
                    Response::Ok().body(body::SizedStream::new(STR.len() as u64, body)),
                )
            }),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: HTTP/2 strips HTTP/1 chunked transfer encoding while delivering a streaming body through OpenSSL.
///
/// HOW:
/// - Return a one-chunk `STR` stream with an explicit `TRANSFER_ENCODING: chunked` header.
/// - Assert success, absence of the transfer-encoding header, and the original response bytes.
#[ntex::test]
async fn test_h2_body_chunked_explicit() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| {
                let body = once(ready(Ok::<_, io::Error>(Bytes::from_static(STR.as_ref()))));
                Ok::<_, io::Error>(
                    Response::Ok()
                        .header(header::TRANSFER_ENCODING, "chunked")
                        .streaming(body),
                )
            }),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));

    // read response
    let bytes = srv.load_body(response).await.unwrap();

    // decode
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

/// STATEMENT: Invalid response headers become an HTTP/2 error response through OpenSSL.
///
/// HOW:
/// - Build a response containing NUL bytes in `CONTENT_TYPE`.
/// - Assert status 500 and the body `Invalid HTTP header value`.
#[ntex::test]
async fn test_h2_response_http_error_handling() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| {
                let broken_header = Bytes::from_static(b"\0\0\0");
                Ok::<_, io::Error>(
                    Response::Ok()
                        .header(header::CONTENT_TYPE, &broken_header[..])
                        .body(STR),
                )
            }),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"Invalid HTTP header value"));
}

/// STATEMENT: A service error becomes its configured HTTP/2 status and body through OpenSSL.
///
/// HOW:
/// - Return `InternalError` containing `error` and `StatusCode::BAD_REQUEST`.
/// - Assert status 400 and the exact `error` response body.
#[ntex::test]
async fn test_h2_service_error() {
    let srv = test_server(async move |_| {
        openssl(
            ssl_acceptor(),
            HttpService::h2(async |_| {
                Err::<Response, _>(InternalError::default("error", StatusCode::BAD_REQUEST))
            }),
        )
    });

    let response = srv.srequest(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"error"));
}

struct SetOnDrop(Arc<AtomicUsize>, Arc<Mutex<Option<::oneshot::Sender<()>>>>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
        let _ = self.1.lock().unwrap().take().unwrap().send(());
    }
}

/// STATEMENT: Dropping an in-flight HTTP/2 request cancels its server handler through OpenSSL.
///
/// HOW:
/// - Run a long-sleeping handler with a `SetOnDrop` guard, wait for its started flag, and drop the request via `select`.
/// - Assert the started branch wins and the handler's drop counter becomes exactly `1`.
#[ntex::test]
async fn test_h2_client_drop() -> io::Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let started = Arc::new(AtomicBool::new(false));
    let started2 = started.clone();
    let (tx, rx) = ::oneshot::async_channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(async move |_| {
        let tx = tx.clone();
        let count = count2.clone();
        let started = started2.clone();
        openssl(
            ssl_acceptor(),
            HttpService::h2(async move |req: Request| {
                let st = SetOnDrop(count.clone(), tx.clone());
                started.store(true, Ordering::Relaxed);

                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_2);
                sleep(Seconds(30)).await;
                drop(st);
                Ok::<_, io::Error>(Response::Ok().build())
            }),
        )
    });

    // drop the request once the handler is running
    let wait_started = async {
        while !started.load(Ordering::Relaxed) {
            sleep(Millis(10)).await;
        }
    };
    let result = timeout(
        Millis(2500),
        select(srv.srequest(Method::GET, "/").send(), wait_started),
    )
    .await;
    assert!(matches!(result, Ok(Either::Right(()))));
    let _ = timeout(Millis(1500), rx).await;
    assert_eq!(count.load(Ordering::Relaxed), 1);
    Ok(())
}

/// STATEMENT: An incomplete OpenSSL TLS handshake closes without application response bytes.
///
/// HOW:
/// - Configure a one-second handshake timeout and open a raw TCP connection without sending a handshake.
/// - Read until closure and assert the resulting application data string is empty.
#[ntex::test]
async fn test_ssl_handshake_timeout() {
    use std::io::Read;

    let srv = test::server_with_config(
        async move |_| {
            openssl(
                ssl_acceptor(),
                HttpService::h2(async |_| Ok::<_, io::Error>(Response::Ok().build())),
            )
        },
        SharedCfg::new("SVC").add(TlsConfig::new().set_handshake_timeout(Seconds(1))),
    );

    let mut stream = std::net::TcpStream::connect(srv.addr()).unwrap();
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.is_empty());
}

/// STATEMENT: A secure WebSocket byte transport echoes binary data and close codes through OpenSSL.
///
/// HOW:
/// - Upgrade an HTTP/1 request into `WsTransport` over TLS and send a binary `text` frame.
/// - Assert the binary echo, then send Away close and assert the same close code is echoed.
#[ntex::test]
async fn test_ws_transport() {
    let srv = test_server(async |_| {
        openssl(
            ssl_acceptor(),
            HttpService::new(async |_| Ok::<_, io::Error>(Response::NotFound())).h1_control(
                async move |req: h1::Control<_, _>| {
                    let ack = if let h1::Control::Upgrade(upg) = req {
                        let (ack, io, req, codec) = upg.handle();

                        // send handshake respone
                        let res = handshake_response(req.head()).build();
                        io.encode(
                            h1::Message::Item((res.drop_body(), body::BodySize::None)),
                            &codec,
                        )
                        .unwrap();

                        // start websocket service
                        let io = ws::WsTransport::create(io, ws::Codec::default());
                        while let Some(item) =
                            io.recv(&BytesCodec).await.map_err(|e| e.into_inner())?
                        {
                            io.send(item, &BytesCodec).await.unwrap()
                        }

                        ack
                    } else {
                        req.ack()
                    };
                    Ok::<_, io::Error>(ack)
                },
            ),
        )
    });

    let io = srv.wss().await.unwrap().into_inner().0;
    let codec = ws::Codec::default().set_client_mode();

    io.send(ws::Message::Binary(Bytes::from_static(b"text")), &codec)
        .await
        .unwrap();

    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    // the transport echoes the close code
    io.send(ws::Message::Close(Some(ws::CloseCode::Away.into())), &codec)
        .await
        .unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Close(Some(ws::CloseCode::Away.into())));
}

/// STATEMENT: Graceful HTTP/2 shutdown waits for active handlers to finish through OpenSSL.
///
/// HOW:
/// - Start two delayed requests and assert both handlers are active before `stop(true)`.
/// - Assert the client tasks observe their expected success and HTTP/2 error outcomes, and the active-handler counter is `0` after shutdown.
#[ntex::test]
async fn test_h2_graceful_shutdown() -> io::Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(async move |_| {
        let tx = tx.clone();
        let count = count2.clone();
        openssl(
            ssl_acceptor(),
            HttpService::h2(async move |_| {
                let count = count.clone();
                count.fetch_add(1, Ordering::Relaxed);
                if count.load(Ordering::Relaxed) == 2 {
                    let _ = tx.lock().unwrap().take().unwrap().send(());
                }
                sleep(Millis(750)).await;
                count.fetch_sub(1, Ordering::Relaxed);
                Ok::<_, io::Error>(Response::Ok().build())
            }),
        )
    });

    let req = srv.srequest(Method::GET, "/");
    rt::spawn(async move {
        assert!(req.send().await.is_ok());
        sleep(Millis(100000)).await;
    });
    let req = srv.srequest(Method::GET, "/");
    rt::spawn(async move {
        assert!(matches!(
            req.send().await.err().unwrap().into_error(),
            client::error::ClientError::H2 { .. }
        ));
        sleep(Millis(100000)).await;
    });
    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 2);

    let (tx, rx) = oneshot::channel();
    rt::spawn(async move {
        srv.stop(true).await;
        let _ = tx.send(());
    });

    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 0);
    Ok(())
}
