use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};
use std::{io, io::Read, io::Write, net};

use futures_util::future::{self, FutureExt};
use futures_util::stream::{once, StreamExt};
use regex::Regex;

use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::test::server as test_server;
use ntex::http::{
    body, HttpService, KeepAlive, Method, Request, Response, StatusCode, Version,
};
use ntex::time::{sleep, timeout, Millis, Seconds};
use ntex::{service::fn_service, util::Bytes, util::Ready, web::error};

#[ntex::test]
async fn test_h1() {
    let srv = test_server(|| {
        HttpService::build()
            .headers_read_rate(Seconds(1), Seconds::ZERO, 256)
            .keep_alive(KeepAlive::Disabled)
            .disconnect_timeout(Seconds(1))
            .h1(|req: Request| {
                assert!(req.peer_addr().is_some());
                Ready::Ok::<_, io::Error>(Response::Ok().finish())
            })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_h1_2() {
    let srv = test_server(|| {
        HttpService::build()
            .keep_alive(KeepAlive::Disabled)
            .disconnect_timeout(Seconds(1))
            .headers_read_rate(Seconds(1), Seconds::ZERO, 256)
            .finish(|req: Request| {
                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_11);
                Ready::Ok::<_, io::Error>(Response::Ok().finish())
            })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // check date
    let hdr = response.header(header::DATE).unwrap();
    assert!(!hdr.to_str().unwrap().starts_with("000"));
}

#[ntex::test]
async fn test_expect_continue() {
    let srv = test_server(|| {
        HttpService::build()
            .expect(fn_service(|req: Request| async move {
                sleep(Millis(20)).await;
                if req.head().uri.query() == Some("yes=") {
                    Ok(req)
                } else {
                    Err(error::InternalError::default(
                        "error",
                        StatusCode::PRECONDITION_FAILED,
                    ))
                }
            }))
            .keep_alive(KeepAlive::Disabled)
            .h1(fn_service(|mut req: Request| async move {
                let _ = req.payload().next().await;
                Ok::<_, io::Error>(Response::Ok().finish())
            }))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test HTTP/1.1\r\nexpect: 100-continue\r\n\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 412 Precondition Failed\r\ncontent-length"));

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"GET /test?yes= HTTP/1.1\r\ncontent-length:4\r\nexpect: 100-continue\r\n\r\n",
    );
    let mut data = [0; 25];
    let _ = stream.read_exact(&mut data[..]);
    assert_eq!(&data, b"HTTP/1.1 100 Continue\r\n\r\n");

    let mut data = String::new();
    let _ = stream.write_all(b"test");
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 200 OK\r\n"));
}

#[ntex::test]
async fn test_chunked_payload() {
    let chunk_sizes = vec![32768, 32, 32768];
    let total_size: usize = chunk_sizes.iter().sum();

    let srv = test_server(|| {
        HttpService::build().h1(fn_service(|mut request: Request| {
            request
                .take_payload()
                .map(|res| match res {
                    Ok(pl) => pl,
                    Err(e) => panic!("Error reading payload: {}", e),
                })
                .fold(0usize, |acc, chunk| async move { acc + chunk.len() })
                .map(|req_size| {
                    Ok::<_, io::Error>(Response::Ok().body(format!("size={}", req_size)))
                })
        }))
    });

    let returned_size = {
        let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
        let _ =
            stream.write_all(b"POST /test HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n");

        for chunk_size in chunk_sizes.iter() {
            let mut bytes = Vec::new();
            let random_bytes: Vec<u8> =
                (0..*chunk_size).map(|_| rand::random::<u8>()).collect();

            bytes.extend(format!("{:X}\r\n", chunk_size).as_bytes());
            bytes.extend(&random_bytes[..]);
            bytes.extend(b"\r\n");
            let _ = stream.write_all(&bytes);
        }
        let _ = stream.write_all(b"0\r\n\r\n");

        let mut data = String::new();
        let _ = stream.read_to_string(&mut data);

        let re = Regex::new(r"size=([0-9]+)").unwrap();
        let size: usize = match re.captures(&data) {
            Some(caps) => caps.get(1).unwrap().as_str().parse().unwrap(),
            None => panic!("Failed to find size in HTTP Response: {}", data),
        };
        size
    };

    assert_eq!(returned_size, total_size);
}

#[ntex::test]
async fn test_slow_request() {
    let srv = test_server(|| {
        HttpService::build()
            .headers_read_rate(Seconds(1), Seconds::ZERO, 256)
            .finish(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"));
}

#[ntex::test]
async fn test_http1_malformed_request() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP1.1\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 400 Bad Request"));
}

#[ntex::test]
async fn test_http1_keepalive() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
}

#[ntex::test]
async fn test_http1_keepalive_timeout() {
    let srv = test_server(|| {
        HttpService::build()
            .keep_alive(1)
            .h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    sleep(Millis(1100)).await;

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

/// Keep-alive must occure only while waiting complete request
#[ntex::test]
async fn test_http1_no_keepalive_during_response() {
    let srv = test_server(|| {
        HttpService::build().keep_alive(1).h1(|_| async {
            sleep(Millis(1100)).await;
            Ok::<_, io::Error>(Response::Ok().finish())
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
}

#[ntex::test]
async fn test_http1_keepalive_close() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nconnection: close\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http10_keepalive_default_close() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.0\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.0 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http10_keepalive() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream
        .write_all(b"GET /test/tests/test HTTP/1.0\r\nconnection: keep-alive\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.0 200 OK\r\n");

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.0\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.0 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http1_keepalive_disabled() {
    let srv = test_server(|| {
        HttpService::build()
            .keep_alive(KeepAlive::Disabled)
            .h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().finish()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_content_length() {
    let srv = test_server(|| {
        HttpService::build().h1(|req: Request| {
            let indx: usize = req.uri().path()[1..].parse().unwrap();
            let statuses = [
                StatusCode::NO_CONTENT,
                StatusCode::CONTINUE,
                StatusCode::SWITCHING_PROTOCOLS,
                StatusCode::PROCESSING,
                StatusCode::OK,
                StatusCode::NOT_FOUND,
            ];
            Ready::Ok::<_, io::Error>(Response::new(statuses[indx]))
        })
    });

    let header = HeaderName::from_static("content-length");
    let value = HeaderValue::from_static("0");

    {
        for i in 0..4 {
            let req = srv.request(Method::GET, format!("/{}", i));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), None);

            let req = srv.request(Method::HEAD, format!("/{}", i));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), None);
        }

        for i in 4..6 {
            let req = srv.request(Method::GET, format!("/{}", i));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), Some(&value));
        }
    }
}

#[ntex::test]
async fn test_h1_headers() {
    let data = STR.repeat(10);
    let data2 = data.clone();

    let mut srv = test_server(move || {
        let data = data.clone();
        HttpService::build().h1(move |_| {
            let mut builder = Response::Ok();
            for idx in 0..20 {
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
            Ready::Ok::<_, io::Error>(builder.body(data.clone()))
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
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
async fn test_h1_body() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h1_head_empty() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv.request(Method::HEAD, "/").send().await.unwrap();
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
async fn test_h1_head_binary() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| {
            Ready::Ok::<_, io::Error>(
                Response::Ok().content_length(STR.len() as u64).body(STR),
            )
        })
    });

    let response = srv.request(Method::HEAD, "/").send().await.unwrap();
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
async fn test_h1_head_binary2() {
    let srv = test_server(|| {
        HttpService::build().h1(|_| Ready::Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv.request(Method::HEAD, "/").send().await.unwrap();
    assert!(response.status().is_success());

    {
        let len = response.headers().get(header::CONTENT_LENGTH).unwrap();
        assert_eq!(format!("{}", STR.len()), len.to_str().unwrap());
    }
}

#[ntex::test]
async fn test_h1_body_length() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| {
            let body = once(Ready::Ok(Bytes::from_static(STR.as_ref())));
            Ready::Ok::<_, io::Error>(
                Response::Ok().body(body::SizedStream::new(STR.len() as u64, body)),
            )
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h1_body_chunked_explicit() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| {
            let body = once(Ready::Ok::<_, io::Error>(Bytes::from_static(STR.as_ref())));
            Ready::Ok::<_, io::Error>(
                Response::Ok()
                    .header(header::TRANSFER_ENCODING, "chunked")
                    .streaming(body),
            )
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response
            .headers()
            .get(header::TRANSFER_ENCODING)
            .unwrap()
            .to_str()
            .unwrap(),
        "chunked"
    );

    // read response
    let bytes = srv.load_body(response).await.unwrap();

    // decode
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h1_body_chunked_implicit() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| {
            let body = once(Ready::Ok::<_, io::Error>(Bytes::from_static(STR.as_ref())));
            Ready::Ok::<_, io::Error>(Response::Ok().streaming(body))
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        response
            .headers()
            .get(header::TRANSFER_ENCODING)
            .unwrap()
            .to_str()
            .unwrap(),
        "chunked"
    );

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h1_response_http_error_handling() {
    let mut srv = test_server(|| {
        HttpService::build().h1(fn_service(|_| {
            let broken_header = Bytes::from_static(b"\0\0\0");
            Ready::Ok::<_, io::Error>(
                Response::Ok()
                    .header(header::CONTENT_TYPE, &broken_header[..])
                    .body(STR),
            )
        }))
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"failed to parse header value"));
}

#[ntex::test]
async fn test_h1_service_error() {
    let mut srv = test_server(|| {
        HttpService::build().h1(|_| {
            future::err::<Response, _>(error::InternalError::default(
                "error",
                StatusCode::BAD_REQUEST,
            ))
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"error"));
}

struct SetOnDrop(Arc<AtomicUsize>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[ntex::test]
async fn test_h1_client_drop() -> io::Result<()> {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();

    let srv = test_server(move || {
        let count = count2.clone();
        HttpService::build().h1(move |req: Request| {
            let count = count.clone();
            async move {
                let _st = SetOnDrop(count);
                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_11);
                sleep(Seconds(100)).await;
                Ok::<_, io::Error>(Response::Ok().finish())
            }
        })
    });

    let result = timeout(Millis(100), srv.request(Method::GET, "/").send()).await;
    assert!(result.is_err());
    sleep(Millis(150)).await;
    assert_eq!(count.load(Ordering::Relaxed), 1);
    Ok(())
}
