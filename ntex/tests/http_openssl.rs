#![cfg(feature = "openssl")]
use std::io;
use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc, Mutex};

use futures_util::stream::{once, Stream, StreamExt};
use tls_openssl::ssl::{AlpnError, SslAcceptor, SslFiletype, SslMethod};

use ntex::codec::BytesCodec;
use ntex::http::error::PayloadError;
use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::test::server as test_server;
use ntex::http::{body, h1, HttpService, Method, Request, Response, StatusCode, Version};
use ntex::service::{fn_service, ServiceFactory};
use ntex::time::{sleep, timeout, Millis, Seconds};
use ntex::util::{Bytes, BytesMut, Ready};
use ntex::{channel::oneshot, rt, web::error::InternalError, ws, ws::handshake_response};

async fn load_body<S>(stream: S) -> Result<BytesMut, PayloadError>
where
    S: Stream<Item = Result<Bytes, PayloadError>>,
{
    let body = stream
        .map(|res| if let Ok(chunk) = res { chunk } else { panic!() })
        .fold(BytesMut::new(), move |mut body, chunk| async move {
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

#[ntex::test]
async fn test_h2() -> io::Result<()> {
    let srv = test_server(move || {
        HttpService::build()
            .h2(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
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
            .h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
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
                Ready::Ok::<_, io::Error>(Response::Ok().finish())
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
                    .map_err(io::Error::other)?;
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
            .h2(|req: Request| async move {
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
                Ok::<_, io::Error>(Response::new(statuses[indx]))
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
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
            Ready::Ok::<_, io::Error>(builder.body(data.clone()))
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
            .h2(|_| async { Ok::<_, io::Error>(Response::Ok().body(STR)) })
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
            .finish(|_| async { Ok::<_, io::Error>(Response::Ok().body(STR)) })
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
            .h2(|_| async {
                Ok::<_, io::Error>(
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

/// Server must send content-length, but no payload
#[ntex::test]
async fn test_h2_head_binary2() {
    let srv = test_server(move || {
        HttpService::build()
            .h2(|_| async { Ok::<_, io::Error>(Response::Ok().body(STR)) })
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
            .h2(|_| async {
                let body = once(Ready::Ok(Bytes::from_static(STR.as_ref())));
                Ok::<_, io::Error>(
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
                let body =
                    once(Ready::Ok::<_, io::Error>(Bytes::from_static(STR.as_ref())));
                Ready::Ok::<_, io::Error>(
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
                Ready::Ok::<_, io::Error>(
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
    assert_eq!(bytes, Bytes::from_static(b"Invalid HTTP header value"));
}

#[ntex::test]
async fn test_h2_service_error() {
    let mut srv = test_server(move || {
        HttpService::build()
            .h2(|_| {
                Ready::Err::<Response, _>(InternalError::default(
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

struct SetOnDrop(Arc<AtomicUsize>, Arc<Mutex<Option<::oneshot::Sender<()>>>>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
        let _ = self.1.lock().unwrap().take().unwrap().send(());
    }
}

#[ntex::test]
async fn test_h2_client_drop() -> io::Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(move || {
        let tx = tx.clone();
        let count = count2.clone();
        HttpService::build()
            .h2(move |req: Request| {
                let st = SetOnDrop(count.clone(), tx.clone());
                async move {
                    assert!(req.peer_addr().is_some());
                    assert_eq!(req.version(), Version::HTTP_2);
                    sleep(Seconds(30)).await;
                    drop(st);
                    Ok::<_, io::Error>(Response::Ok().finish())
                }
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let result = timeout(Millis(1500), srv.srequest(Method::GET, "/").send()).await;
    assert!(result.is_err());
    let _ = timeout(Millis(1500), rx).await;
    assert_eq!(count.load(Ordering::Relaxed), 1);
    Ok(())
}

#[ntex::test]
async fn test_ssl_handshake_timeout() {
    use std::io::Read;

    let srv = test_server(move || {
        HttpService::build()
            .ssl_handshake_timeout(Seconds(1))
            .h2(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let mut stream = std::net::TcpStream::connect(srv.addr()).unwrap();
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.is_empty());
}

#[ntex::test]
async fn test_ws_transport() {
    let mut srv = test_server(|| {
        HttpService::build()
            .h1_control(|req: h1::Control<_, _>| async move {
                let ack = if let h1::Control::Upgrade(upg) = req {
                    upg.handle(|req, io, codec| async move {
                        let res = handshake_response(req.head()).finish();

                        // send handshake respone
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
                            io.send(item.freeze(), &BytesCodec).await.unwrap()
                        }

                        Ok::<_, io::Error>(())
                    })
                } else {
                    req.ack()
                };
                Ok::<_, io::Error>(ack)
            })
            .finish(|_| Ready::Ok::<_, io::Error>(Response::NotFound()))
            .openssl(ssl_acceptor())
    });

    let io = srv.wss().await.unwrap().into_inner().0;
    let codec = ws::Codec::default().client_mode();

    io.send(ws::Message::Binary(Bytes::from_static(b"text")), &codec)
        .await
        .unwrap();

    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(item, ws::Frame::Binary(Bytes::from_static(b"text")));

    io.send(ws::Message::Close(None), &codec).await.unwrap();
    let item = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(
        item,
        ws::Frame::Close(Some(ws::CloseReason {
            code: ws::CloseCode::Normal,
            description: None
        }))
    );
}

#[ntex::test]
async fn test_h2_graceful_shutdown() -> io::Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(move || {
        let tx = tx.clone();
        let count = count2.clone();
        HttpService::build()
            .h2(move |_| {
                let count = count.clone();
                count.fetch_add(1, Ordering::Relaxed);
                if count.load(Ordering::Relaxed) == 2 {
                    let _ = tx.lock().unwrap().take().unwrap().send(());
                }
                async move {
                    sleep(Millis(1000)).await;
                    count.fetch_sub(1, Ordering::Relaxed);
                    Ok::<_, io::Error>(Response::Ok().finish())
                }
            })
            .openssl(ssl_acceptor())
            .map_err(|_| ())
    });

    let req = srv.srequest(Method::GET, "/");
    rt::spawn(async move {
        let _ = req.send().await.unwrap();
        sleep(Millis(100000)).await;
    });
    let req = srv.srequest(Method::GET, "/");
    rt::spawn(async move {
        let _ = req.send().await.unwrap();
        sleep(Millis(100000)).await;
    });
    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 2);

    let (tx, rx) = oneshot::channel();
    rt::spawn(async move {
        srv.stop().await;
        let _ = tx.send(());
    });

    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 0);
    Ok(())
}
