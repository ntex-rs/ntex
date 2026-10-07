use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{future::ready, io, io::Read, io::Write, net};

use futures_util::{future::FutureExt, stream::StreamExt, stream::once};
use regex::Regex;

use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::{
    HttpService, HttpServiceConfig, KeepAlive, Method, Request, Response, StatusCode, Version,
};
use ntex::http::{body, h1, h1::Control, test, test::server as test_server};
use ntex::time::{Millis, Seconds, sleep};
use ntex::{SharedCfg, channel::oneshot, fn_service, rt, util::Bytes, web::error};

/// Upper limit for blocking socket reads, a correct server answers much sooner.
const TEN_SECONDS: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits until `check` returns `true`, checking it every 10 milliseconds.
///
/// Returns an error if it does not happen within five seconds.
async fn wait_for(check: impl Fn() -> bool) -> Result<(), &'static str> {
    for _ in 0..500 {
        if check() {
            return Ok(());
        }
        sleep(Millis(10)).await;
    }
    Err("the expected condition is not met in time")
}

#[ntex::test]
async fn test_h1() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::h1(async |req: Request| {
                assert!(req.peer_addr().is_some());
                Ok::<_, io::Error>(Response::Ok().build())
            })
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_headers_read_rate(Seconds(1), Seconds::ZERO, 256)
                .set_keepalive(KeepAlive::Disabled),
        ),
    );

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());
}

#[ntex::test]
async fn test_h1_2() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::new(async |req: Request| {
                assert!(req.peer_addr().is_some());
                assert_eq!(req.version(), Version::HTTP_11);
                Ok::<_, io::Error>(Response::Ok().build())
            })
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_headers_read_rate(Seconds(1), Seconds::ZERO, 256)
                .set_keepalive(KeepAlive::Disabled),
        ),
    );

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // check date
    let hdr = response.header(header::DATE).unwrap();
    assert!(!hdr.to_str().unwrap().starts_with("000"));
}

#[ntex::test]
async fn test_expect_continue() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::h1(async move |mut req: Request| {
                let _ = req.payload().next().await;
                Ok::<_, io::Error>(Response::Ok().build())
            })
            .control(async move |req: Control<_, _>| {
                sleep(Millis(20)).await;
                let ack = if let Control::Expect(exc) = req {
                    if exc
                        .get_ref()
                        .head()
                        .uri
                        .query()
                        .is_some_and(|q| q == "yes=")
                    {
                        exc.ack()
                    } else {
                        exc.fail(error::InternalError::default(
                            "error",
                            StatusCode::PRECONDITION_FAILED,
                        ))
                    }
                } else {
                    req.ack()
                };
                Ok::<_, std::convert::Infallible>(ack)
            })
        },
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_keepalive(KeepAlive::Disabled)),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ =
        stream.write_all(b"GET /test HTTP/1.1\r\nhost: localhost\r\nexpect: 100-continue\r\n\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 412 Precondition Failed\r\ncontent-length"));

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream
        .write_all(b"GET /test?yes= HTTP/1.1\r\nhost: localhost\r\ncontent-length:4\r\nexpect: 100-continue\r\n\r\n");
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
    let chunk_sizes = [32768, 32, 32768];
    let total_size: usize = chunk_sizes.iter().sum();

    let srv = test_server(async |_| {
        HttpService::h1(fn_service(|mut request: Request| {
            request
                .take_payload()
                .map(|res| match res {
                    Ok(pl) => pl,
                    Err(e) => panic!("Error reading payload: {e}"),
                })
                .fold(0usize, async move |acc, chunk| acc + chunk.len())
                .map(|req_size| Ok::<_, io::Error>(Response::Ok().body(format!("size={req_size}"))))
        }))
    });

    let returned_size = {
        let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
        let _ = stream.write_all(
            b"POST /test HTTP/1.1\r\nhost: localhost\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n",
        );

        for chunk_size in chunk_sizes.iter() {
            let mut bytes = Vec::new();
            let random_bytes: Vec<u8> = (0..*chunk_size).map(|_| rand::random::<u8>()).collect();

            bytes.extend(format!("{chunk_size:X}\r\n").as_bytes());
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
            None => panic!("Failed to find size in HTTP Response: {data}"),
        };
        size
    };

    assert_eq!(returned_size, total_size);
}

#[ntex::test]
async fn test_slow_request() {
    const DATA: &[u8] = b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n";

    let srv = test::server_with_config(
        async |_| HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_headers_read_rate(
            Seconds(1),
            Seconds(2),
            4,
        )),
    );

    // reading the head above the rate extends the timeout, the cumulative
    // budget caps it, the head is never completed
    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let _ = stream.write_all(&DATA[..5]);

    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let mut stream = stream.try_clone().unwrap();
        let done = done.clone();
        // five bytes every 300 milliseconds is above the four bytes per
        // second rate, it keeps extending the timeout
        std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                if stream.write_all(b"aaaaa").is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        })
    };

    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    done.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"), "{data:?}");

    // a stalled head is not extended, the read rate is not reached
    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let _ = stream.write_all(&DATA[..4]);
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"), "{data:?}");
}

#[ntex::test]
async fn test_slow_request2() {
    const DATA: &[u8] = b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n";

    let srv = test::server_with_config(
        async |_| HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        // a single read rate period, extending it is covered by `test_slow_request`
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_headers_read_rate(
            Seconds(1),
            Seconds(1),
            4,
        )),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    let _ = stream.write_all(DATA);
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 408 Request Timeout"), "{data:?}");
}

#[ntex::test]
async fn test_headers_read_rate_extends_timeout() {
    let srv = test::server_with_config(
        async |_| HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_headers_read_rate(
            Seconds(1),
            Seconds(3),
            4,
        )),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /");
    sleep(Millis(1100)).await;
    let _ = stream.write_all(b" HTTP/1.1\r\nhost: localhost\r\n\r\n");

    let mut data = vec![0; 1024];
    let len = stream.read(&mut data).unwrap();
    assert!(data[..len].starts_with(b"HTTP/1.1 200 OK\r\n"));
}

#[ntex::test]
async fn test_headers_read_rate_counts_parsed_bytes() {
    let srv = test::server_with_config(
        async |_| HttpService::new(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_headers_read_rate(
            Seconds(2),
            Seconds(5),
            4,
        )),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    // The complete request line is consumed by the decoder, leaving no bytes
    // buffered while the remainder of the header block is still pending.
    let _ = stream.write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\n");
    sleep(Millis(3200)).await;

    stream.set_nonblocking(true).unwrap();
    let mut data = vec![0; 1024];
    match stream.read(&mut data) {
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => (),
        Ok(len) => panic!(
            "server responded before the request head completed: {:?}",
            String::from_utf8_lossy(&data[..len])
        ),
        Err(err) => panic!("failed to inspect the connection: {err}"),
    }
    stream.set_nonblocking(false).unwrap();

    let _ = stream.write_all(b"\r\n");

    let len = stream.read(&mut data).unwrap();
    assert!(data[..len].starts_with(b"HTTP/1.1 200 OK\r\n"));
}

#[ntex::test]
async fn test_payload_read_rate_extends_timeout() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::new(async |mut req: Request| {
                while req.payload().recv().await.is_some() {}
                Ok::<_, io::Error>(Response::Ok().build())
            })
        },
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_payload_read_rate(
            Seconds(1),
            Seconds(3),
            4,
        )),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ =
        stream.write_all(b"POST / HTTP/1.1\r\nhost: localhost\r\ncontent-length: 10\r\n\r\n12345");
    sleep(Millis(1100)).await;
    let _ = stream.write_all(b"67890");

    let mut data = vec![0; 1024];
    let len = stream.read(&mut data).unwrap();
    assert!(data[..len].starts_with(b"HTTP/1.1 200 OK\r\n"));
}

#[ntex::test]
async fn test_http1_malformed_request() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP1.1\r\n");
    let mut data = String::new();
    let _ = stream.read_to_string(&mut data);
    assert!(data.starts_with("HTTP/1.1 400 Bad Request"));
}

#[ntex::test]
async fn test_http1_keepalive() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
}

#[ntex::test]
async fn test_http1_keepalive_timeout() {
    let srv = test::server_with_config(
        async |_| HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_keepalive(1)),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    // the connection is closed once the keep-alive timeout expires
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

/// Keep-alive must occure only while waiting complete request
#[ntex::test]
async fn test_http1_no_keepalive_during_response() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::h1(async |req: Request| {
                // the second request is processed while the keep-alive timer
                // of the previous wait is left armed
                if req.uri().path() == "/slow" {
                    sleep(Millis(1200)).await;
                }
                Ok::<_, io::Error>(Response::Ok().build())
            })
        },
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_keepalive(1)),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    for path in ["/fast", "/slow"] {
        let _ =
            stream.write_all(format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n\r\n").as_bytes());
        let mut data = vec![0; 1024];
        let _ = stream.read(&mut data);
        assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    }
}

/// Keep-alive timeout after sending response
#[ntex::test]
async fn test_http1_keepalive_after_response() {
    let ka = Arc::new(AtomicBool::new(false));
    let ka2 = ka.clone();
    let srv = test::server_with_config(
        async move |_| {
            let ka = ka2.clone();
            HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build())).control(
                async move |req: Control<_, _>| {
                    if let Control::Disconnect(h1::control::Reason::KeepAlive(_)) = &req {
                        ka.store(true, Ordering::Release);
                    }
                    Ok::<_, std::convert::Infallible>(req.ack())
                },
            )
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_headers_read_rate(Seconds(1), Seconds(2), 4)
                .set_keepalive(1),
        ),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    // the connection is closed once the keep-alive timeout expires
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let mut data = vec![0; 1024];
    let len = stream.read(&mut data).unwrap();
    assert_eq!(len, 0);
    assert!(ka.load(Ordering::Relaxed));
}

#[ntex::test]
async fn test_http1_keepalive_close() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n",
    );
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http10_keepalive_default_close() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.0\r\n\r\n");
    let mut data = vec![0; 1024];
    let n = stream.read(&mut data).unwrap();
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    // an HTTP/1.1 response is persistent by default
    assert!(data[..n].windows(19).any(|w| w == b"connection: close\r\n"));

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http10_keepalive() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.0\r\nconnection: keep-alive\r\n\r\n");
    let mut data = vec![0; 1024];
    let n = stream.read(&mut data).unwrap();
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    assert!(
        data[..n]
            .windows(24)
            .any(|w| w == b"connection: keep-alive\r\n")
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.0\r\n\r\n");
    let mut data = vec![0; 1024];
    let n = stream.read(&mut data).unwrap();
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
    // an HTTP/1.1 response is persistent by default
    assert!(data[..n].windows(19).any(|w| w == b"connection: close\r\n"));

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

#[ntex::test]
async fn test_http1_keepalive_disabled() {
    let srv = test::server_with_config(
        async |_| HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build())),
        SharedCfg::new("SRV").add(HttpServiceConfig::new().set_keepalive(KeepAlive::Disabled)),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");

    let mut data = vec![0; 1024];
    let res = stream.read(&mut data).unwrap();
    assert_eq!(res, 0);
}

/// Payload timer should not fire aftre dispatcher has read whole payload
#[ntex::test]
async fn test_http1_disable_payload_timer_after_whole_pl_has_been_read() {
    let srv = test::server_with_config(
        async |_| {
            HttpService::h1(async move |mut req: Request| {
                req.payload().recv().await;
                sleep(Millis(1500)).await;
                Ok::<_, io::Error>(Response::Ok().build())
            })
            .control(async |msg: Control<_, _>| Ok::<_, io::Error>(msg.ack()))
        },
        SharedCfg::new("SRV").add(
            HttpServiceConfig::new()
                .set_headers_read_rate(Seconds(1), Seconds(1), 128)
                .set_payload_read_rate(Seconds(1), Seconds(1), 512)
                .set_keepalive(1),
        ),
    );

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream
        .write_all(b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n");
    // the head and the payload arrive as separate reads, so payload timing
    // starts for an incomplete payload
    sleep(Millis(100)).await;
    let _ = stream.write_all(b"\r\n");
    sleep(Millis(100)).await;
    let _ = stream.write_all(b"1234");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
}

/// Handle not consumed payload
#[cfg(unix)]
#[ntex::test]
async fn test_http1_handle_not_consumed_payload() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().build())).control(
            async |msg: Control<_, _>| {
                if matches!(
                    msg,
                    Control::Disconnect(ntex::http::h1::control::Reason::ProtocolError(_))
                ) {
                    panic!()
                }
                Ok::<_, io::Error>(msg.ack())
            },
        )
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\ncontent-length: 4\r\n\r\n",
    );
    sleep(Millis(250)).await;
    let _ = stream.write_all(b"1234");
    let mut data = vec![0; 1024];
    let _ = stream.read(&mut data);
    assert_eq!(&data[..17], b"HTTP/1.1 200 OK\r\n");
}

/// Handle payload errors (keep-alive, disconnects)
#[ntex::test]
async fn test_http1_handle_payload_errors() {
    let started = Arc::new(AtomicUsize::new(0));
    let started2 = started.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();

    let srv = test_server(async move |_| {
        let started = started2.clone();
        let count = count2.clone();
        HttpService::h1(move |mut req: Request| {
            let started = started.clone();
            let count = count.clone();
            async move {
                let mut pl = req.take_payload();
                started.fetch_add(1, Ordering::Release);
                let result = pl.recv().await;
                if result.unwrap().is_err() {
                    count.fetch_add(1, Ordering::Relaxed);
                }
                Ok::<_, io::Error>(Response::Ok().build())
            }
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"GET /test/tests/test HTTP/1.1\r\nhost: localhost\r\ncontent-length: 99999\r\n\r\n",
    );
    // the request is dropped while the handler waits for the payload
    wait_for(|| started.load(Ordering::Acquire) == 1)
        .await
        .expect("request is not received");
    drop(stream);
    wait_for(|| count.load(Ordering::Acquire) == 1)
        .await
        .expect("payload error is not reported");
}

#[ntex::test]
async fn test_content_length() {
    let srv = test_server(async |_| {
        HttpService::h1(async |req: Request| {
            let indx: usize = req.uri().path().as_str()[1..].parse().unwrap();
            let statuses = [
                StatusCode::NO_CONTENT,
                StatusCode::CONTINUE,
                StatusCode::SWITCHING_PROTOCOLS,
                StatusCode::PROCESSING,
                StatusCode::OK,
                StatusCode::NOT_FOUND,
            ];
            Ok::<_, io::Error>(Response::new(statuses[indx]))
        })
    });

    let header = HeaderName::from_static("content-length");
    let value = HeaderValue::from_static("0");

    {
        // interim `1xx` responses cannot be final, they are replaced with 500
        for i in [1, 3] {
            for method in ["GET", "HEAD"] {
                let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
                let _ = stream.write_all(
                    format!("{method} /{i} HTTP/1.1\r\nhost: localhost\r\n\r\n").as_bytes(),
                );
                let mut data = vec![0; 1024];
                let n = stream.read(&mut data).unwrap();
                let data = String::from_utf8_lossy(&data[..n]).to_lowercase();
                assert!(data.starts_with("http/1.1 500"), "{data:?}");
                assert!(data.contains("content-length: 0\r\n"), "{data:?}");
            }
        }

        for i in [0, 2] {
            let req = srv.request(Method::GET, format!("/{i}"));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), None);

            let req = srv.request(Method::HEAD, format!("/{i}"));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), None);
        }

        for i in 4..6 {
            let req = srv.request(Method::GET, format!("/{i}"));
            let response = req.send().await.unwrap();
            assert_eq!(response.headers().get(&header), Some(&value));
        }
    }
}

#[ntex::test]
async fn test_h1_headers() {
    let data = STR.repeat(10);
    let data2 = data.clone();

    let srv = test_server(async move |_| {
        let data = data.clone();
        HttpService::h1(async move |_| {
            let mut builder = Response::Ok();
            for idx in 0..20 {
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().body(STR)))
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert!(response.status().is_success());

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(STR.as_ref()));
}

#[ntex::test]
async fn test_h1_head_empty() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().body(STR)))
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            Ok::<_, io::Error>(Response::Ok().content_length(STR.len() as u64).body(STR))
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| Ok::<_, io::Error>(Response::Ok().body(STR)))
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            let body = once(ready(Ok(Bytes::from_static(STR.as_ref()))));
            Ok::<_, io::Error>(Response::Ok().body(body::SizedStream::new(STR.len() as u64, body)))
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            let body = once(ready(Ok::<_, io::Error>(Bytes::from_static(STR.as_ref()))));
            Ok::<_, io::Error>(
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            let body = once(ready(Ok::<_, io::Error>(Bytes::from_static(STR.as_ref()))));
            Ok::<_, io::Error>(Response::Ok().streaming(body))
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
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            let broken_header = Bytes::from_static(b"\0\0\0");
            Ok::<_, io::Error>(
                Response::Ok()
                    .header(header::CONTENT_TYPE, &broken_header[..])
                    .body(STR),
            )
        })
    });

    let response = srv.request(Method::GET, "/").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // read response
    let bytes = srv.load_body(response).await.unwrap();
    assert_eq!(bytes, Bytes::from_static(b"Invalid HTTP header value"));
}

#[ntex::test]
async fn test_h1_service_error() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_| {
            Err::<Response, _>(error::InternalError::default(
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

// /// If client drops connection, server must drop pending handling futures
// #[ntex::test]
// async fn test_h1_client_drop() -> io::Result<()> {
// struct SetOnDrop(Arc<AtomicUsize>, Option<::oneshot::Sender<()>>);
// impl Drop for SetOnDrop {
//     fn drop(&mut self) {
//         self.0.fetch_add(1, Ordering::Relaxed);
//         let _ = self.1.take().unwrap().send(());
//     }
// }
//     let count = Arc::new(AtomicUsize::new(0));
//     let count2 = count.clone();
//     let (tx, rx) = ::oneshot::channel();
//     let tx = Arc::new(Mutex::new(Some(tx)));

//     let srv = test_server(async move |_| {
//         let tx = tx.clone();
//         let count = count2.clone();
//         HttpService::h1(async move |req: Request| {
//             let tx = tx.clone();
//             let count = count.clone();

//             let _st = SetOnDrop(count, tx.lock().unwrap().take());
//             assert!(req.peer_addr().is_some());
//             assert_eq!(req.version(), Version::HTTP_11);

//             // on connection close, server must drop pending handling future
//             sleep(Millis(150000)).await;
//             Ok::<_, io::Error>(Response::Ok().build())
//         })
//     });

//     let result = timeout(Millis(2500), srv.request(Method::GET, "/").send()).await;
//     assert!(result.is_err());
//     let _ = rx.await;
//     assert_eq!(count.load(Ordering::Relaxed), 1);
//     Ok(())
// }

#[ntex::test]
async fn test_h1_gracefull_shutdown() {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let release = Arc::new(AtomicBool::new(false));
    let release2 = release.clone();
    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(async move |_| {
        let tx = tx.clone();
        let count = count2.clone();
        let release = release2.clone();
        HttpService::h1(async move |_: Request| {
            let count = count.clone();
            let release = release.clone();
            count.fetch_add(1, Ordering::Relaxed);
            if count.load(Ordering::Relaxed) == 2 {
                let _ = tx.lock().unwrap().take().unwrap().send(());
            }

            // the request stays in flight until the test releases it
            while !release.load(Ordering::Acquire) {
                sleep(Millis(10)).await;
            }
            count.fetch_sub(1, Ordering::Relaxed);
            Ok::<_, io::Error>(Response::Ok().build())
        })
    });

    let mut stream1 = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream1.write_all(b"GET /index.html HTTP/1.1\r\nhost: localhost\r\n\r\n");

    let mut stream2 = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream2.write_all(b"GET /index.html HTTP/1.1\r\nhost: localhost\r\n\r\n");

    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 2);

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped2 = stopped.clone();
    let (tx, rx) = oneshot::channel();
    rt::spawn(async move {
        srv.stop(true).await;
        stopped2.store(true, Ordering::Release);
        let _ = tx.send(());
    });

    // shutdown does not complete while the requests are in flight
    sleep(Millis(300)).await;
    assert!(
        !stopped.load(Ordering::Acquire),
        "shutdown did not wait for in-flight requests"
    );
    assert_eq!(count.load(Ordering::Relaxed), 2);

    release.store(true, Ordering::Release);
    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

struct FailingControl;

impl<St, F, E> ntex::ServiceFactory<St, Control<F, E>> for FailingControl
where
    F: ntex::io::Filter,
    E: ntex::http::ResponseError,
{
    type Res = h1::ControlAck<F>;
    type Error = io::Error;
    type Service = FailingControl;
    type InitError = io::Error;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Err(io::Error::other("control init failed"))
    }
}

impl<St, F, E> ntex::Service<St, Control<F, E>> for FailingControl
where
    F: ntex::io::Filter,
    E: ntex::http::ResponseError,
{
    type Res = h1::ControlAck<F>;
    type Error = io::Error;

    async fn call(
        &self,
        req: Control<F, E>,
        _: ntex::Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(req.ack())
    }
}

#[ntex::test]
async fn test_h1_control_init_error_does_not_block_shutdown() {
    let srv = test_server(async |_| {
        HttpService::new(async |_: Request| Ok::<_, io::Error>(Response::Ok().build()))
            .h1_control(FailingControl)
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET /index.html HTTP/1.1\r\n\r\n");
    let mut data = Vec::new();
    let _ = stream.read_to_end(&mut data);
    assert!(data.is_empty());

    let res = ntex::time::timeout(Seconds(5), srv.stop(true)).await;
    assert!(
        res.is_ok(),
        "graceful shutdown waited for leaked connection"
    );
}

#[ntex::test]
async fn test_h1_gracefull_shutdown_2() {
    let count = Arc::new(AtomicUsize::new(0));
    let count2 = count.clone();
    let release = Arc::new(AtomicBool::new(false));
    let release2 = release.clone();
    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let srv = test_server(async move |_| {
        let tx = tx.clone();
        let count = count2.clone();
        let release = release2.clone();
        HttpService::new(async move |_: Request| {
            let count = count.clone();
            let release = release.clone();
            count.fetch_add(1, Ordering::Relaxed);
            if count.load(Ordering::Relaxed) == 2 {
                let _ = tx.lock().unwrap().take().unwrap().send(());
            }

            // the request stays in flight until the test releases it
            while !release.load(Ordering::Acquire) {
                sleep(Millis(10)).await;
            }
            count.fetch_sub(1, Ordering::Relaxed);
            Ok::<_, io::Error>(Response::Ok().build())
        })
    });

    let mut stream1 = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream1.write_all(b"GET /index.html HTTP/1.1\r\nhost: localhost\r\n\r\n");

    let mut stream2 = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream2.write_all(b"GET /index.html HTTP/1.1\r\nhost: localhost\r\n\r\n");

    let _ = rx.await;
    assert_eq!(count.load(Ordering::Acquire), 2);

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped2 = stopped.clone();
    let (tx, rx) = oneshot::channel();
    rt::spawn(async move {
        srv.stop(true).await;
        stopped2.store(true, Ordering::Release);
        let _ = tx.send(());
    });

    // shutdown does not complete while the requests are in flight
    sleep(Millis(300)).await;
    assert!(
        !stopped.load(Ordering::Acquire),
        "shutdown did not wait for in-flight requests"
    );
    assert_eq!(count.load(Ordering::Relaxed), 2);

    release.store(true, Ordering::Release);
    let _ = rx.await;
    assert_eq!(count.load(Ordering::Relaxed), 0);
}

#[ntex::test]
async fn test_h2_request_body_dropped_after_response_resets_stream() {
    use ntex::http::{HeaderMap, Payload};
    use ntex::util::stream_recv;
    use ntex_h2::{MessageKind, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |mut req: Request| {
            // the request body is held after the response and dropped later
            let mut pl: Payload = req.take_payload();
            rt::spawn(async move {
                let _ = stream_recv(&mut pl).await;
                sleep(Millis(200)).await;
                drop(pl);
            });
            Ok::<_, io::Error>(Response::Ok().body("ok"))
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let (snd, rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    snd.send_payload(Bytes::from_static(b"chunk"), false)
        .await
        .unwrap();

    // complete response, the request body is still open
    let mut eof = false;
    while let Some(msg) = rcv.recv().await {
        match msg.kind {
            MessageKind::Headers { eof: true, .. } | MessageKind::Eof(_) => {
                eof = true;
                break;
            }
            _ => (),
        }
    }
    assert!(eof);

    // no data frame follows, the stream is reset when the body is dropped
    let mut reset = false;
    for _ in 0..100 {
        if snd
            .send_payload(Bytes::from_static(b"chunk"), false)
            .await
            .is_err()
        {
            reset = true;
            break;
        }
        sleep(Millis(50)).await;
    }
    assert!(reset, "request stream is not reset");
}

/// The request payload reports the stream reset error and ends after it,
/// the next read returns `None`.
#[ntex::test]
async fn test_h2_request_payload_ends_after_error() {
    use ntex::http::HeaderMap;
    use ntex::util::stream_recv;
    use ntex_h2::client::SimpleClient;

    let result = Arc::new(Mutex::new(None));
    let result2 = result.clone();
    let srv = test_server(async move |_| {
        let result = result2.clone();
        HttpService::h2(move |mut req: Request| {
            let result = result.clone();
            async move {
                // the request body outlives the handler
                let mut pl = req.take_payload();
                rt::spawn(async move {
                    let mut items = Vec::new();
                    loop {
                        match ntex::time::timeout(Millis(500), stream_recv(&mut pl)).await {
                            Ok(Some(Ok(chunk))) => items.push(format!("{chunk:?}")),
                            Ok(Some(Err(e))) => items.push(format!("error: {e:?}")),
                            Ok(None) => break,
                            Err(()) => {
                                items.push("timeout".to_string());
                                break;
                            }
                        }
                    }
                    *result.lock().unwrap() = Some(items);
                });
                Ok::<_, io::Error>(Response::Ok().body("ok"))
            }
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    snd.send_payload(Bytes::from_static(b"chunk"), false)
        .await
        .unwrap();
    sleep(Millis(50)).await;
    snd.reset(ntex_h2::frame::Reason::CANCEL);

    let mut items = None;
    for _ in 0..100 {
        sleep(Millis(20)).await;
        items = result.lock().unwrap().take();
        if items.is_some() {
            break;
        }
    }
    // the stream reset error is reported, not a generic incomplete payload
    let items = items.expect("request body is not completed");
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0], "b\"chunk\"");
    assert!(
        items[1].starts_with("error:") && items[1].contains("CANCEL"),
        "{items:?}"
    );
}

/// A malformed request is a stream error, the connection stays open.
#[ntex::test]
async fn test_h2_malformed_request_uri() {
    use ntex::http::HeaderMap;
    use ntex_h2::{MessageKind, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |_: Request| Ok::<_, io::Error>(Response::Ok().body("ok")))
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    for (method, eof) in [(Method::GET, true), (Method::POST, false)] {
        let (_snd, rcv) = client
            .send(method, "/a b".into(), HeaderMap::default(), eof)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status, Some(StatusCode::BAD_REQUEST));
        assert!(eof);
    }
    assert!(!client.is_closed());

    let (_snd, rcv) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), true)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::OK));
}

/// A response body error resets only its stream, the connection stays open.
#[ntex::test]
async fn test_h2_response_body_error_resets_stream() {
    use ntex::http::HeaderMap;
    use ntex_h2::{MessageKind, StreamEof, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |req: Request| {
            if req.path() == "/err" {
                Ok::<_, io::Error>(Response::Ok().streaming(Box::pin(once(async {
                    Err::<Bytes, _>(io::Error::other("body error"))
                }))))
            } else {
                // the response is sent after the other stream fails
                sleep(Millis(200)).await;
                Ok(Response::Ok().body("ok"))
            }
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let (_snd1, rcv1) = client
        .send(Method::GET, "/slow".into(), HeaderMap::default(), true)
        .await
        .unwrap();
    let (_snd2, rcv2) = client
        .send(Method::GET, "/err".into(), HeaderMap::default(), true)
        .await
        .unwrap();

    let msg = rcv2.recv().await.unwrap();
    let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::OK));
    assert!(!eof);
    let msg = rcv2.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(StreamEof::Error(ref e)) if format!("{e:?}").contains("INTERNAL_ERROR")),
        "{msg:?}"
    );

    // the other stream completes
    let msg = rcv1.recv().await.unwrap();
    let MessageKind::Headers { pseudo, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::OK));
    let msg = rcv1.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(StreamEof::Data(ref d, _)) if d == "ok"),
        "{msg:?}"
    );
    assert!(!client.is_closed());
}

/// `304 Not Modified` response has no body and no `content-length: 0`.
#[ntex::test]
async fn test_h2_not_modified_has_no_body() {
    use ntex::http::HeaderMap;
    use ntex_h2::{MessageKind, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |req: Request| {
            let mut res = Response::builder(StatusCode::NOT_MODIFIED);
            Ok::<_, io::Error>(match req.path() {
                "/sized" => res.body("body"),
                "/stream" => res.streaming(Box::pin(once(async {
                    Ok::<_, io::Error>(Bytes::from_static(b"body"))
                }))),
                _ => res.build(),
            })
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    for path in ["/empty", "/sized", "/stream"] {
        let (_snd, rcv) = client
            .send(Method::GET, path.into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers {
            pseudo,
            headers,
            eof,
        } = msg.kind
        else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status, Some(StatusCode::NOT_MODIFIED), "{path}");
        assert!(eof, "{path}: body is sent");
        assert!(
            !headers.contains_key(header::CONTENT_LENGTH),
            "{path}: {headers:?}"
        );
    }
}

/// A sized body ends the stream with its last data frame, a streaming body with an empty one.
#[ntex::test]
async fn test_h2_response_body_end_stream() {
    use ntex::http::HeaderMap;
    use ntex_h2::{MessageKind, StreamEof, client::SimpleClient};

    fn chunks<E>() -> futures_util::stream::Iter<std::array::IntoIter<Result<Bytes, E>, 2>> {
        futures_util::stream::iter([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"def")),
        ])
    }

    let srv = test_server(async |_| {
        HttpService::h2(async |req: Request| {
            Ok::<_, io::Error>(if req.path() == "/sized" {
                Response::Ok().body(body::SizedStream::new(6, chunks()))
            } else {
                Response::Ok().streaming(chunks::<io::Error>())
            })
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    for (path, last) in [("/sized", "def"), ("/stream", "")] {
        let (_snd, rcv) = client
            .send(Method::GET, path.into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers {
            pseudo, headers, ..
        } = msg.kind
        else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status, Some(StatusCode::OK), "{path}");
        assert!(headers.contains_key(header::DATE), "{path}: {headers:?}");
        if path == "/sized" {
            assert_eq!(headers.get(header::CONTENT_LENGTH).unwrap(), "6");
        }

        let msg = rcv.recv().await.unwrap();
        assert!(
            matches!(msg.kind, MessageKind::Data(ref d, _) if d == "abc"),
            "{path}: {msg:?}"
        );
        if path == "/stream" {
            let msg = rcv.recv().await.unwrap();
            assert!(
                matches!(msg.kind, MessageKind::Data(ref d, _) if d == "def"),
                "{path}: {msg:?}"
            );
        }
        let msg = rcv.recv().await.unwrap();
        assert!(
            matches!(msg.kind, MessageKind::Eof(StreamEof::Data(ref d, _)) if d == last),
            "{path}: {msg:?}"
        );
    }
}

/// Raw HTTP/2 connection preface and a `POST /` request without END_STREAM.
fn h2_raw_post() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    // SETTINGS
    buf.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    // HEADERS, END_HEADERS: `:method POST`, `:scheme http`, `:path /`
    buf.extend_from_slice(&[0, 0, 3, 1, 4, 0, 0, 0, 1, 0x83, 0x86, 0x84]);
    buf
}

/// Raw HTTP/2 DATA frame for stream 1.
fn h2_raw_data(buf: &mut Vec<u8>, data: &[u8], eof: bool) {
    #[allow(clippy::cast_possible_truncation)]
    buf.extend_from_slice(&[0, 0, data.len() as u8, 0, u8::from(eof), 0, 0, 0, 1]);
    buf.extend_from_slice(data);
}

/// Empty non-final DATA frames are not flow controlled, they are not queued
/// as request body chunks.
#[ntex::test]
async fn test_h2_empty_data_frames_are_not_queued() {
    use ntex::util::stream_recv;

    let chunks = Arc::new(Mutex::new(None));
    let chunks2 = chunks.clone();
    let srv = test_server(async move |_| {
        let chunks = chunks2.clone();
        HttpService::h2(move |mut req: Request| {
            let chunks = chunks.clone();
            async move {
                let mut pl = req.take_payload();
                let mut items = Vec::new();
                while let Some(chunk) = stream_recv(&mut pl).await {
                    items.push(chunk.unwrap());
                }
                *chunks.lock().unwrap() = Some(items);
                Ok::<_, io::Error>(Response::Ok().body("ok"))
            }
        })
    });

    // a non-empty DATA frame resets the count of consecutive empty frames
    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let mut buf = h2_raw_post();
    for _ in 0..9 {
        h2_raw_data(&mut buf, b"", false);
    }
    h2_raw_data(&mut buf, b"a", false);
    for _ in 0..9 {
        h2_raw_data(&mut buf, b"", false);
    }
    h2_raw_data(&mut buf, b"bc", true);
    stream.write_all(&buf).unwrap();

    wait_for(|| chunks.lock().unwrap().is_some())
        .await
        .expect("request is not completed");
    assert_eq!(
        chunks.lock().unwrap().take().unwrap(),
        vec![Bytes::from("a"), Bytes::from("bc")]
    );

    // the connection stays open
    let frames = h2_raw_read_frames(&mut stream);
    assert!(
        !frames.0.iter().any(|f| f.0 == 7),
        "GOAWAY is sent: {frames:?}"
    );
    assert!(!frames.1, "connection is closed");
}

/// Request trailers are available after the request payload is complete.
#[ntex::test]
async fn test_h2_request_trailers() {
    use ntex::util::stream_recv;

    let result = Arc::new(Mutex::new(None));
    let result2 = result.clone();
    let srv = test_server(async move |_| {
        let result = result2.clone();
        HttpService::h2(move |mut req: Request| {
            let result = result.clone();
            async move {
                let mut pl = req.take_payload();
                let mut body = Vec::new();
                while let Some(chunk) = stream_recv(&mut pl).await {
                    body.extend_from_slice(&chunk.unwrap());
                }
                *result.lock().unwrap() = Some((body, pl.trailers()));
                Ok::<_, io::Error>(Response::Ok().body("ok"))
            }
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let mut buf = h2_raw_post();
    h2_raw_data(&mut buf, b"abc", false);
    // HEADERS, END_STREAM | END_HEADERS: `x-trailer: 1`
    buf.extend_from_slice(&[0, 0, 13, 1, 5, 0, 0, 0, 1, 0, 9]);
    buf.extend_from_slice(b"x-trailer");
    buf.extend_from_slice(&[1, b'1']);
    stream.write_all(&buf).unwrap();

    wait_for(|| result.lock().unwrap().is_some())
        .await
        .expect("request is not completed");
    let (body, trailers) = result.lock().unwrap().take().unwrap();
    assert_eq!(body, b"abc");
    let trailers = trailers.expect("trailers are not received");
    assert_eq!(trailers.len(), 1);
    assert_eq!(trailers.get("x-trailer").unwrap(), "1");
}

/// Reads frames until the connection is closed or idle for 500ms,
/// returns frame types with payloads and `true` if the connection is closed.
fn h2_raw_read_frames(stream: &mut net::TcpStream) -> (Vec<(u8, Vec<u8>)>, bool) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let mut data = Vec::new();
    let mut buf = [0; 1024];
    let closed = loop {
        match stream.read(&mut buf) {
            Ok(0) => break true,
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break true,
            Err(_) => break false,
        }
    };
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 9 <= data.len() {
        let len = (usize::from(data[pos]) << 16)
            | (usize::from(data[pos + 1]) << 8)
            | usize::from(data[pos + 2]);
        let end = (pos + 9 + len).min(data.len());
        frames.push((data[pos + 3], data[pos + 9..end].to_vec()));
        pos += 9 + len;
    }
    (frames, closed)
}

/// The connection is closed with `ENHANCE_YOUR_CALM` after 10 consecutive
/// empty non-final DATA frames.
#[ntex::test]
async fn test_h2_empty_data_frames_limit() {
    use ntex::util::stream_recv;

    let body = Arc::new(Mutex::new(None));
    let body2 = body.clone();
    let srv = test_server(async move |_| {
        let body = body2.clone();
        HttpService::h2(move |mut req: Request| {
            let body = body.clone();
            async move {
                let mut pl = req.take_payload();
                let mut items = Vec::new();
                while let Some(chunk) = stream_recv(&mut pl).await {
                    items.push(chunk.map_err(|_| ()));
                }
                *body.lock().unwrap() = Some(items);
                Ok::<_, io::Error>(Response::Ok().body("ok"))
            }
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let mut buf = h2_raw_post();
    for _ in 0..10 {
        h2_raw_data(&mut buf, b"", false);
    }
    // frames after the limit are not delivered
    h2_raw_data(&mut buf, b"abc", true);
    stream.write_all(&buf).unwrap();

    // the server sends GOAWAY and closes the connection
    let (frames, closed) = h2_raw_read_frames(&mut stream);
    let goaway = frames
        .iter()
        .find(|f| f.0 == 7)
        .unwrap_or_else(|| panic!("GOAWAY is not sent: {frames:?}"));
    // error code is ENHANCE_YOUR_CALM
    assert_eq!(goaway.1[4..8], [0, 0, 0, 0xb], "GOAWAY: {goaway:?}");
    assert!(closed, "connection is not closed");
    sleep(Millis(100)).await;
    assert_ne!(
        body.lock().unwrap().take(),
        Some(vec![Ok(Bytes::from("abc"))]),
        "request body is delivered"
    );
}

/// The control service handles `Expect: 100-continue`, the default ack sends `100 Continue`.
#[ntex::test]
async fn test_h2_expect_continue() {
    use ntex::http::{HeaderMap, h2};
    use ntex::util::{BytesMut, stream_recv};
    use ntex_h2::{MessageKind, StreamEof, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |mut req: Request| {
            assert_eq!(req.path(), "/");
            let mut pl = req.take_payload();
            let mut body = BytesMut::new();
            while let Some(chunk) = stream_recv(&mut pl).await {
                body.extend_from_slice(&chunk.unwrap());
            }
            Ok::<_, io::Error>(Response::Ok().body(body.freeze()))
        })
        .control(async |msg: h2::Control<_>| {
            Ok::<_, io::Error>(match msg {
                h2::Control::Expect(expect)
                    if expect.pseudo().path.as_deref() == Some("/reject") =>
                {
                    expect.fail(StatusCode::EXPECTATION_FAILED, HeaderMap::new())
                }
                msg => msg.ack(),
            })
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let mut hdrs = HeaderMap::default();
    hdrs.insert(header::EXPECT, HeaderValue::from_static("100-Continue"));

    let (snd, rcv) = client
        .send(Method::POST, "/".into(), hdrs.clone(), false)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::CONTINUE));
    assert!(!eof);

    snd.send_payload(Bytes::from_static(b"body"), true)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::OK));
    // the sized body ends the stream with its last data frame
    let msg = rcv.recv().await.unwrap();
    assert!(
        matches!(msg.kind, MessageKind::Eof(StreamEof::Data(ref d, _)) if d == "body"),
        "{msg:?}"
    );

    // the expectation is rejected, the final response is sent without `100 Continue`
    let (_snd, rcv) = client
        .send(Method::POST, "/reject".into(), hdrs, false)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, eof, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::EXPECTATION_FAILED));
    assert!(eof);
    assert!(!client.is_closed());
}

/// Failure of the control service resets the stream, the connection stays open.
#[ntex::test]
async fn test_h2_expect_control_error() {
    use ntex::http::{HeaderMap, h2};
    use ntex_h2::{MessageKind, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |_: Request| Ok::<_, io::Error>(Response::Ok().build())).control(
            async |msg: h2::Control<_>| match msg {
                h2::Control::Expect(expect)
                    if expect.pseudo().path.as_deref() == Some("/error") =>
                {
                    Err(io::Error::other("control error"))
                }
                msg => Ok(msg.ack()),
            },
        )
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let mut hdrs = HeaderMap::default();
    hdrs.insert(header::EXPECT, HeaderValue::from_static("100-Continue"));

    let (_snd, rcv) = client
        .send(Method::POST, "/error".into(), hdrs, false)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Eof(ntex_h2::StreamEof::Error(err)) = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert!(
        matches!(
            &*err,
            ntex_h2::StreamError::Reset(ntex_h2::frame::Reason::INTERNAL_ERROR)
        ),
        "{err:?}"
    );

    // the connection is still usable
    let (_snd, rcv) = client
        .send(Method::GET, "/".into(), HeaderMap::default(), true)
        .await
        .unwrap();
    let msg = rcv.recv().await.unwrap();
    let MessageKind::Headers { pseudo, .. } = msg.kind else {
        panic!("unexpected message: {msg:?}")
    };
    assert_eq!(pseudo.status, Some(StatusCode::OK));
}

/// Informational response of the application cannot complete the request.
#[ntex::test]
async fn test_h2_informational_response_is_replaced() {
    use ntex::http::HeaderMap;
    use ntex_h2::{MessageKind, client::SimpleClient};

    let srv = test_server(async |_| {
        HttpService::h2(async |req: Request| {
            let status = if req.path() == "/101" {
                StatusCode::SWITCHING_PROTOCOLS
            } else {
                StatusCode::CONTINUE
            };
            Ok::<_, io::Error>(Response::new(status))
        })
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    for path in ["/100", "/101"] {
        let (_snd, rcv) = client
            .send(Method::GET, path.into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let msg = rcv.recv().await.unwrap();
        let MessageKind::Headers { pseudo, .. } = msg.kind else {
            panic!("unexpected message: {msg:?}")
        };
        assert_eq!(pseudo.status, Some(StatusCode::INTERNAL_SERVER_ERROR));
    }
}

#[ntex::test]
async fn test_h1_request_line_too_long() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_: Request| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let req = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(16 * 1024));
    let _ = stream.write_all(req.as_bytes());
    let mut data = vec![0; 1024];
    let n = stream.read(&mut data).unwrap();
    assert!(data[..n].starts_with(b"HTTP/1.1 414"));

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let req = format!(
        "GET /{} HTTP/1.1\r\nhost: a\r\n\r\n",
        "a".repeat(16 * 1024 - 20)
    );
    let _ = stream.write_all(req.as_bytes());
    let n = stream.read(&mut data).unwrap();
    assert!(data[..n].starts_with(b"HTTP/1.1 200"));
}

#[ntex::test]
async fn test_h1_unsupported_transfer_coding() {
    let srv = test_server(async |_| {
        HttpService::h1(async |_: Request| Ok::<_, io::Error>(Response::Ok().build()))
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: gzip, chunked\r\n\r\n0\r\n\r\n",
    );
    let mut data = vec![0; 1024];
    let n = stream.read(&mut data).unwrap();
    assert!(data[..n].starts_with(b"HTTP/1.1 501"), "{:?}", &data[..n]);

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(
        b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked, gzip\r\n\r\n0\r\n\r\n",
    );
    let n = stream.read(&mut data).unwrap();
    assert!(data[..n].starts_with(b"HTTP/1.1 400"), "{:?}", &data[..n]);
}

/// Chunked request trailers are available after the request payload is complete.
#[ntex::test]
async fn test_h1_request_trailers() {
    use ntex::util::stream_recv;

    let srv = test_server(async |_| {
        HttpService::h1(async |mut req: Request| {
            let mut pl = req.take_payload();
            let mut body = Vec::new();
            while let Some(chunk) = stream_recv(&mut pl).await {
                body.extend_from_slice(&chunk.unwrap());
            }
            let trailers = pl.trailers().map(|t| {
                t.iter()
                    .map(|(n, v)| format!("{n}={}", v.to_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(",")
            });
            Ok::<_, io::Error>(Response::Ok().body(format!(
                "{}|{}",
                String::from_utf8(body).unwrap(),
                trailers.unwrap_or_default()
            )))
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    stream
        .write_all(
            b"POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\n\r\n\
              3\r\nabc\r\n0\r\nx-trailer: 1\r\n\r\n\
              POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: chunked\r\n\r\n\
              2\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut data = Vec::new();
    let mut buf = [0; 1024];
    // read until both pipelined responses are received
    while !data.ends_with(b"\r\n\r\nde|") {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
    let data = String::from_utf8(data).unwrap();
    assert!(data.contains("\r\n\r\nabc|x-trailer=1"), "{data:?}");
    assert!(data.ends_with("\r\n\r\nde|"), "{data:?}");
}

/// A failed transport write stops polling an always ready response body.
#[ntex::test]
async fn test_h1_body_not_polled_after_peer_reset() {
    use std::task::{Context, Poll};

    static DATA: [u8; 16 * 1024] = [b'a'; 16 * 1024];
    const MAX_POLLS: usize = 5_000_000;

    struct Stream(Arc<AtomicUsize>);
    impl body::MessageBody for Stream {
        fn size(&self) -> body::BodySize {
            body::BodySize::Stream
        }
        fn poll_next_chunk(
            &mut self,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Bytes, std::rc::Rc<dyn std::error::Error>>>> {
            if self.0.fetch_add(1, Ordering::Relaxed) >= MAX_POLLS {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(Ok(Bytes::from_static(&DATA))))
            }
        }
    }

    let polls = Arc::new(AtomicUsize::new(0));
    let polls2 = polls.clone();
    let srv = test_server(async move |_| {
        let polls = polls2.clone();
        HttpService::h1(move |_: Request| {
            let body = body::Body::from_message(Stream(polls.clone()));
            async move { Ok::<_, io::Error>(Response::Ok().body(body)) }
        })
    });

    let mut stream = net::TcpStream::connect(srv.addr()).unwrap();
    let _ = stream.write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n");
    let mut data = vec![0; 64 * 1024];
    let mut total = 0;
    while total < 16 * 1024 * 1024 {
        total += stream.read(&mut data).unwrap();
    }
    // closing with unread data resets the connection
    drop(stream);

    // the poll count stabilizes once the body is not polled anymore
    let mut polls1 = polls.load(Ordering::Relaxed);
    for _ in 0..80 {
        sleep(Millis(25)).await;
        let current = polls.load(Ordering::Relaxed);
        if current == polls1 {
            break;
        }
        polls1 = current;
    }
    sleep(Millis(300)).await;
    let polls2 = polls.load(Ordering::Relaxed);
    assert_eq!(polls1, polls2);
    assert!(polls2 < MAX_POLLS, "{polls2}");
}
