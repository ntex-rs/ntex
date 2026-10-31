use std::sync::{Arc, Mutex, atomic::AtomicBool, atomic::AtomicUsize, atomic::Ordering};
use std::{cell::Cell, convert::Infallible, future::poll_fn, rc::Rc, task::Poll};
use std::{future::ready, io, io::Read, io::Write, net};

use futures_util::{future::FutureExt, stream::StreamExt, stream::once};
use regex::Regex;

use ntex::http::header::{self, HeaderName, HeaderValue};
use ntex::http::{
    HttpService, HttpServiceConfig, KeepAlive, Method, Request, Response, StatusCode, Version,
};
use ntex::http::{body, h1, h1::Control, test, test::server as test_server};
use ntex::service::{BindCtx, Ctx, ReadyNotifier};
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

/// STATEMENT: `HttpService::h1` serves requests with the peer address available.
///
/// HOW:
/// - Start an HTTP/1 service with a header read deadline and keep-alive disabled.
/// - Send a `GET`, assert the handler sees a peer address, and verify a successful response.
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

/// STATEMENT: `HttpService::new` recognizes HTTP/1.1 requests and supplies a nonzero date year.
///
/// HOW:
/// - Send a `GET` and assert the handler sees a peer address and `Version::HTTP_11`.
/// - Verify a successful response whose `Date` header does not start with a zero year.
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

/// STATEMENT: HTTP/1 expectation control can reject a request or permit its body transmission.
///
/// HOW:
/// - Reject an `Expect: 100-continue` request without the accepted query and verify status `412`.
/// - Accept the query, verify `100 Continue`, then send the body and verify `200 OK`.
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

/// STATEMENT: HTTP/1 chunked request decoding delivers the complete payload across chunk sizes.
///
/// HOW:
/// - Send random payload chunks of 32768, 32, and 32768 bytes followed by the terminating chunk.
/// - Sum decoded bytes in the handler and verify the response reports the total sent.
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

/// STATEMENT: Incomplete HTTP/1 headers time out when progress stalls or the total budget expires.
///
/// HOW:
/// - Configure a one-second read period, a two-second budget, and a four-byte minimum rate.
/// - Keep an incomplete head above the minimum rate and verify it still receives `408`.
/// - Stall a second incomplete head below the minimum rate and verify another `408`.
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

/// STATEMENT: Header read deadlines apply to later requests on an HTTP/1 keep-alive connection.
///
/// HOW:
/// - Configure a single one-second header read period and complete an initial request.
/// - Verify `200 OK`, then send an incomplete second head and verify `408 Request Timeout`.
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

/// STATEMENT: Sufficient header progress extends the HTTP/1 read deadline.
///
/// HOW:
/// - Send five bytes with a four-byte minimum rate and a one-second initial read period.
/// - Complete the head after 1.1 seconds, within the total budget, and verify `200 OK`.
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

/// STATEMENT: Header read-rate accounting includes bytes already consumed by the HTTP/1 decoder.
///
/// HOW:
/// - Send a complete request line and a header, leaving only the final blank line outstanding.
/// - Wait beyond the initial read period and verify no response or connection close occurs.
/// - Finish the head within the extended budget and verify `200 OK`.
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

/// STATEMENT: Sufficient payload progress extends the HTTP/1 body read deadline.
///
/// HOW:
/// - Send half a ten-byte body with a four-byte minimum rate and a one-second read period.
/// - Send the remaining bytes after 1.1 seconds and verify the consuming handler returns `200 OK`.
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

/// STATEMENT: HTTP/1 rejects a malformed protocol version in the request line.
///
/// HOW:
/// - Send a raw request line containing `HTTP1.1` instead of `HTTP/1.1`.
/// - Read the response and verify it starts with `400 Bad Request`.
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

/// STATEMENT: HTTP/1.1 connections accept successive requests with default keep-alive settings.
///
/// HOW:
/// - Send a complete request over a raw TCP connection and verify `200 OK`.
/// - Reuse that connection for another request and verify a second `200 OK`.
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

/// STATEMENT: An idle HTTP/1 connection closes after its keep-alive timeout.
///
/// HOW:
/// - Configure a one-second keep-alive timeout and verify a request receives `200 OK`.
/// - Leave the connection idle and verify the next socket read returns EOF.
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

/// STATEMENT: Keep-alive expiration does not interrupt an active HTTP/1 response.
///
/// HOW:
/// - Configure a one-second keep-alive timeout and serve a fast request.
/// - Reuse the connection for a handler delayed by 1.2 seconds and verify both responses are `200`.
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

/// STATEMENT: Idle HTTP/1 closure after a response reports a keep-alive disconnect.
///
/// HOW:
/// - Enable header read deadlines and a one-second keep-alive timeout.
/// - Verify a request succeeds, then wait for socket EOF.
/// - Assert the control service observed `Control::Disconnect` with `Reason::KeepAlive`.
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

/// STATEMENT: HTTP/1.1 honors a request's `Connection: close` directive.
///
/// HOW:
/// - Send a raw request containing `Connection: close` and verify `200 OK`.
/// - Verify the following socket read returns EOF.
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

/// STATEMENT: HTTP/1.0 requests close by default even when the response uses HTTP/1.1.
///
/// HOW:
/// - Send an HTTP/1.0 request without a connection directive.
/// - Verify an HTTP/1.1 `200 OK` with `Connection: close`, followed by socket EOF.
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

/// STATEMENT: HTTP/1.0 connection headers reflect explicit keep-alive or default closure.
///
/// HOW:
/// - Send an HTTP/1.0 request with `Connection: keep-alive` and verify the response echoes it.
/// - On another connection, omit that directive and verify `Connection: close` followed by EOF.
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

/// STATEMENT: Disabling keep-alive closes HTTP/1.1 connections after their response.
///
/// HOW:
/// - Configure `KeepAlive::Disabled` and send a request without `Connection: close`.
/// - Verify `200 OK` followed by socket EOF.
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

/// STATEMENT: A completed HTTP/1 payload no longer triggers its read deadline.
///
/// HOW:
/// - Configure a one-second payload deadline and deliver the head and body in separate reads.
/// - Read the body in the handler, delay the response by 1.5 seconds, and verify `200 OK`.
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

/// STATEMENT: An HTTP/1 handler may respond without consuming a delayed request payload.
///
/// HOW:
/// - Install a handler that ignores the body and a control service that panics on protocol errors.
/// - Send headers declaring a four-byte body, deliver it after a delay, and verify `200 OK`.
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

/// STATEMENT: Disconnecting an HTTP/1 client reports an error to its pending payload reader.
///
/// HOW:
/// - Declare a large body without sending it and wait until the handler starts reading.
/// - Drop the client connection and verify the handler records one payload error.
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

/// STATEMENT: HTTP/1 response framing respects status-specific content-length rules.
///
/// HOW:
/// - Return `100` and `102` for `GET` and `HEAD`; verify replacement `500` responses with length zero.
/// - Return `204` and `101` for both methods and verify no `Content-Length` header is sent.
/// - Return empty `200` and `404` responses to `GET` and verify `Content-Length: 0`.
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

/// STATEMENT: Large HTTP/1 response headers do not corrupt the response body.
///
/// HOW:
/// - Return twenty long custom headers together with a body containing ten copies of `STR`.
/// - Verify success and compare the fully received body with the original bytes.
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

/// STATEMENT: HTTP/1 delivers a fixed response body unchanged.
///
/// HOW:
/// - Return `STR` from the request handler and send a `GET`.
/// - Verify success and compare the complete received body with `STR`.
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

/// STATEMENT: HTTP/1 `HEAD` responses retain the computed body length without sending body bytes.
///
/// HOW:
/// - Return `STR` without an explicit length and send a `HEAD` request.
/// - Verify success, `Content-Length` equal to `STR.len()`, and an empty received body.
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

/// STATEMENT: HTTP/1 `HEAD` preserves an explicit content length while suppressing the body.
///
/// HOW:
/// - Return `STR` with an explicitly configured content length and send a `HEAD`.
/// - Verify success, the configured `Content-Length`, and an empty received body.
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

/// STATEMENT: HTTP/1 computes the content length for a fixed-body `HEAD` response.
///
/// HOW:
/// - Return `STR` without explicitly configuring a length and send a `HEAD`.
/// - Verify success and a `Content-Length` equal to `STR.len()`.
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

/// STATEMENT: HTTP/1 delivers the complete contents of a sized response stream.
///
/// HOW:
/// - Wrap one chunk containing `STR` in a `body::SizedStream` with the matching length.
/// - Send a `GET`, verify success, and compare the received body with `STR`.
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

/// STATEMENT: HTTP/1 supports an explicitly chunked streaming response.
///
/// HOW:
/// - Stream one chunk containing `STR` and explicitly set `Transfer-Encoding: chunked`.
/// - Verify success, the chunked encoding header, and a decoded body equal to `STR`.
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

/// STATEMENT: HTTP/1 automatically selects chunked encoding for an unsized response stream.
///
/// HOW:
/// - Stream one chunk containing `STR` without setting a transfer-encoding header.
/// - Verify success, an added `Transfer-Encoding: chunked`, and the complete decoded body.
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

/// STATEMENT: An invalid HTTP/1 response header becomes an internal-server-error response.
///
/// HOW:
/// - Build a response with a `Content-Type` value containing null bytes.
/// - Verify status `500` and the body `Invalid HTTP header value`.
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

/// STATEMENT: HTTP/1 converts a service error into its configured response status and body.
///
/// HOW:
/// - Return an `InternalError` configured with `StatusCode::BAD_REQUEST` and the message `error`.
/// - Verify the client receives status `400` and that exact body.
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

// /// STATEMENT: Dropping an HTTP/1 client request drops its pending server handler.
// ///
// /// HOW:
// /// - Delay the handler beyond the client timeout and attach a drop counter and notification.
// /// - Verify the request times out, await the drop notification, and assert one handler drop.
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

/// STATEMENT: Graceful shutdown of `HttpService::h1` waits for active requests.
///
/// HOW:
/// - Hold two handlers on separate connections and start `srv.stop(true)`.
/// - Verify shutdown remains pending while both requests are active.
/// - Release the handlers, await shutdown completion, and verify the active count reaches zero.
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

/// STATEMENT: HTTP/1 control-service initialization failure does not leak a shutdown blocker.
///
/// HOW:
/// - Use `FailingControl` to reject control-service creation and verify the connection sends no data.
/// - Request graceful shutdown and verify it completes within five seconds.
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

/// STATEMENT: Graceful shutdown of `HttpService::new` waits for active HTTP/1 requests.
///
/// HOW:
/// - Hold two HTTP/1 handlers on separate connections and start `srv.stop(true)`.
/// - Verify shutdown remains pending while both requests are active.
/// - Release the handlers, await shutdown completion, and verify the active count reaches zero.
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

struct HandlerReadinessGate {
    available: Cell<bool>,
    changed: ReadyNotifier,
    binds: Arc<AtomicUsize>,
    pending: Arc<AtomicBool>,
}

impl ntex::Service<(), ntex::web::WebRequest<()>> for HandlerReadinessGate {
    type Res = ntex::web::WebRequest<()>;
    type Error = Infallible;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.binds.fetch_add(1, Ordering::Relaxed);
        ctx.subscribe(&self.changed);
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Infallible> {
        poll_fn(|_| {
            if self.available.get() {
                Poll::Ready(Ok(()))
            } else {
                self.pending.store(true, Ordering::Release);
                Poll::Pending
            }
        })
        .await
    }

    async fn call(
        &self,
        req: ntex::web::WebRequest<()>,
        _: Ctx<'_, Self>,
    ) -> Result<Self::Res, Infallible> {
        Ok(req)
    }
}

async fn h1_initialized_handler_readiness(path: &'static str) {
    use std::io::{BufRead, BufReader};

    use ntex::service::{apply_fn_factory, fn_factory};
    use ntex::web::{self, App, WebRequest};

    fn read_response(stream: &mut BufReader<net::TcpStream>) {
        let mut line = String::new();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).unwrap(), 0);
            if line == "\r\n" {
                break;
            }
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(AtomicUsize::new(0));
    let binds = Arc::new(AtomicUsize::new(0));
    let blocked = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicBool::new(false));
    let calls2 = calls.clone();
    let heads2 = heads.clone();
    let binds2 = binds.clone();
    let blocked2 = blocked.clone();
    let pending2 = pending.clone();
    let (block_tx, block_rx) = ::oneshot::channel();
    let (resume_tx, resume_rx) = ::oneshot::channel();
    let control = Arc::new(Mutex::new(Some((block_rx, resume_rx))));

    let srv = test_server(async move |_| {
        let gate = Rc::new(HandlerReadinessGate {
            available: Cell::new(true),
            changed: ReadyNotifier::new(),
            binds: binds2.clone(),
            pending: pending2.clone(),
        });
        let handler = ntex::factory(fn_factory({
            let gate = gate.clone();
            async move |(): &()| Ok::<_, Infallible>(gate.clone())
        }))
        .map(|req: WebRequest<()>| req.into_response(Response::Ok()));
        let app = App::new()
            .service(web::service("/blocked").build(handler.clone()))
            .service(web::resource("/ready").to(async || Response::Ok()))
            .service(
                web::resource("/resource")
                    .route(web::get().to(async || Response::Ok()))
                    .default_service(handler.clone()),
            )
            .default_service(handler);

        let blocked = blocked2.clone();
        let (block_rx, resume_rx) = control.lock().unwrap().take().unwrap();
        rt::spawn(async move {
            if block_rx.await.is_ok() {
                gate.available.set(false);
                gate.changed.notify();
                blocked.store(true, Ordering::Release);
                if resume_rx.await.is_ok() {
                    gate.available.set(true);
                    gate.changed.notify();
                }
            }
        });

        let calls = calls2.clone();
        let heads = heads2.clone();
        HttpService::h1(apply_fn_factory(app, async move |req: Request, ctx| {
            calls.fetch_add(1, Ordering::Relaxed);
            ctx.call(req).await
        }))
        .control(async move |event: Control<_, _>| {
            if matches!(event, Control::Request(_)) {
                heads.fetch_add(1, Ordering::Relaxed);
            }
            Ok::<_, Infallible>(event.ack())
        })
    });

    let stream = net::TcpStream::connect(srv.addr()).unwrap();
    stream.set_read_timeout(Some(TEN_SECONDS)).unwrap();
    let mut stream = BufReader::new(stream);
    for path in ["/ready", path, "/ready"] {
        write!(
            stream.get_mut(),
            "GET {path} HTTP/1.1\r\nhost: localhost\r\n\r\n"
        )
        .unwrap();
        read_response(&mut stream);
    }
    assert_eq!(binds.load(Ordering::Relaxed), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(heads.load(Ordering::Relaxed), 3);

    block_tx.send(()).unwrap();
    wait_for(|| blocked.load(Ordering::Acquire)).await.unwrap();
    stream
        .get_mut()
        .write_all(
            b"GET /ready HTTP/1.1\r\nhost: localhost\r\n\r\n\
              GET /ready HTTP/1.1\r\nhost: localhost\r\n\r\n",
        )
        .unwrap();
    wait_for(|| pending.load(Ordering::Acquire))
        .await
        .expect("the dispatcher did not check the initialized handler's readiness");
    stream
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    let result = stream.read(&mut [0; 1]);
    assert!(
        matches!(result, Err(ref err)
            if matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
        "an unrelated request responded or disconnected while {path} was not ready: {result:?}"
    );
    assert_eq!(
        heads.load(Ordering::Relaxed),
        3,
        "the dispatcher decoded a new request while {path} was not ready"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "the dispatcher entered the application while {path} was not ready"
    );

    resume_tx.send(()).unwrap();
    stream
        .get_ref()
        .set_read_timeout(Some(TEN_SECONDS))
        .unwrap();
    read_response(&mut stream);
    read_response(&mut stream);
    assert_eq!(heads.load(Ordering::Relaxed), 5);
    assert_eq!(calls.load(Ordering::Relaxed), 5);
    assert_eq!(binds.load(Ordering::Relaxed), 1);
}

/// STATEMENT: An initialized route's readiness gates HTTP/1 dispatch for other routes.
///
/// HOW:
/// - Initialize resource zero and a healthy route on one HTTP/1.1 connection, then cache readiness.
/// - Notify that resource zero is unavailable and pipeline two requests to the healthy route.
/// - Observe pending readiness with no response, new `Control::Request`, or application entry.
/// - Notify recovery and verify both queued requests complete without rebinding the handler.
#[ntex::test]
async fn test_h1_initialized_route_readiness_blocks_dispatch() {
    h1_initialized_handler_readiness("/blocked").await;
}

/// STATEMENT: An initialized default handler's readiness gates HTTP/1 dispatch to matching routes.
///
/// HOW:
/// - Initialize the fallback and a healthy route on one HTTP/1.1 connection, then cache readiness.
/// - Notify that the fallback is unavailable and pipeline two requests to the healthy route.
/// - Observe pending readiness with no response, new `Control::Request`, or application entry.
/// - Notify recovery and verify both queued requests complete without rebinding the handler.
#[ntex::test]
async fn test_h1_initialized_default_readiness_blocks_dispatch() {
    h1_initialized_handler_readiness("/missing").await;
}

/// STATEMENT: An initialized resource fallback gates HTTP/1 dispatch even when never selected.
///
/// HOW:
/// - Match a resource's `GET` route to initialize its fallback without invoking it.
/// - Mark that fallback unavailable and pipeline two requests to an unrelated healthy route.
/// - Observe pending readiness with no response, new `Control::Request`, or application entry.
/// - Notify recovery and verify both requests complete without rebinding the fallback.
#[ntex::test]
async fn test_h1_initialized_resource_fallback_readiness_blocks_dispatch() {
    h1_initialized_handler_readiness("/resource").await;
}

async fn h2_initialized_handler_readiness(path: &'static str) {
    use ntex::http::HeaderMap;
    use ntex::service::{apply_fn_factory, fn_factory};
    use ntex::time::timeout;
    use ntex::web::{self, App, WebRequest};
    use ntex_h2::{MessageKind, client::SimpleClient};

    let calls = Arc::new(AtomicUsize::new(0));
    let binds = Arc::new(AtomicUsize::new(0));
    let blocked = Arc::new(AtomicBool::new(false));
    let pending = Arc::new(AtomicBool::new(false));
    let calls2 = calls.clone();
    let binds2 = binds.clone();
    let blocked2 = blocked.clone();
    let pending2 = pending.clone();
    let (block_tx, block_rx) = ::oneshot::channel();
    let (resume_tx, resume_rx) = ::oneshot::channel();
    let control = Arc::new(Mutex::new(Some((block_rx, resume_rx))));

    let srv = test_server(async move |_| {
        let gate = Rc::new(HandlerReadinessGate {
            available: Cell::new(true),
            changed: ReadyNotifier::new(),
            binds: binds2.clone(),
            pending: pending2.clone(),
        });
        let handler = ntex::factory(fn_factory({
            let gate = gate.clone();
            async move |(): &()| Ok::<_, Infallible>(gate.clone())
        }))
        .map(|req: WebRequest<()>| req.into_response(Response::Ok()));
        let app = App::new()
            .service(web::service("/blocked").build(handler.clone()))
            .service(web::resource("/ready").to(async || Response::Ok()))
            .service(
                web::resource("/resource")
                    .route(web::get().to(async || Response::Ok()))
                    .default_service(handler.clone()),
            )
            .default_service(handler);

        let blocked = blocked2.clone();
        let (block_rx, resume_rx) = control.lock().unwrap().take().unwrap();
        rt::spawn(async move {
            if block_rx.await.is_ok() {
                gate.available.set(false);
                gate.changed.notify();
                blocked.store(true, Ordering::Release);
                if resume_rx.await.is_ok() {
                    gate.available.set(true);
                    gate.changed.notify();
                }
            }
        });

        let calls = calls2.clone();
        HttpService::h2(apply_fn_factory(app, async move |req: Request, ctx| {
            calls.fetch_add(1, Ordering::Relaxed);
            ctx.call(req).await
        }))
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    for path in ["/ready", path, "/ready"] {
        let (_snd, rcv) = client
            .send(Method::GET, path.into(), HeaderMap::default(), true)
            .await
            .unwrap();
        let msg = timeout(Seconds(5), rcv.recv()).await.unwrap().unwrap();
        assert!(
            matches!(msg.kind, MessageKind::Headers { ref pseudo, eof: true, .. }
                if pseudo.status == Some(StatusCode::OK)),
            "{path}: {msg:?}"
        );
    }
    assert_eq!(binds.load(Ordering::Relaxed), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 3);

    block_tx.send(()).unwrap();
    wait_for(|| blocked.load(Ordering::Acquire)).await.unwrap();
    let (_first, first) = client
        .send(Method::GET, "/ready".into(), HeaderMap::default(), true)
        .await
        .unwrap();
    wait_for(|| pending.load(Ordering::Acquire))
        .await
        .expect("the dispatcher did not check the initialized handler's readiness");
    let (_second, second) = client
        .send(Method::GET, "/ready".into(), HeaderMap::default(), true)
        .await
        .unwrap();
    for rcv in [&first, &second] {
        assert!(
            timeout(Millis(100), rcv.recv()).await.is_err(),
            "an unrelated request completed while {path} was not ready"
        );
    }
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "the dispatcher entered the application while {path} was not ready"
    );

    resume_tx.send(()).unwrap();
    for rcv in [&first, &second] {
        let msg = timeout(Seconds(5), rcv.recv()).await.unwrap().unwrap();
        assert!(
            matches!(msg.kind, MessageKind::Headers { ref pseudo, eof: true, .. }
                if pseudo.status == Some(StatusCode::OK)),
            "{msg:?}"
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 5);
    assert_eq!(binds.load(Ordering::Relaxed), 1);
}

/// STATEMENT: An initialized route's readiness gates HTTP/2 application dispatch for other routes.
///
/// HOW:
/// - Initialize resource zero and a healthy route on one HTTP/2 connection, then cache readiness.
/// - Notify that resource zero is unavailable and send two requests to the healthy route.
/// - Observe pending readiness, no responses, and no application entry until recovery is notified.
/// - Verify both queued requests complete without rebinding the blocked handler.
#[ntex::test]
async fn test_h2_initialized_route_readiness_blocks_dispatch() {
    h2_initialized_handler_readiness("/blocked").await;
}

/// STATEMENT: An initialized default handler's readiness gates HTTP/2 dispatch to matching routes.
///
/// HOW:
/// - Initialize the fallback and a healthy route on one HTTP/2 connection, then cache readiness.
/// - Notify that the fallback is unavailable and send two requests matching the healthy route.
/// - Observe pending readiness, no responses, and no application entry until recovery is notified.
/// - Verify both queued requests complete without rebinding the default handler.
#[ntex::test]
async fn test_h2_initialized_default_readiness_blocks_dispatch() {
    h2_initialized_handler_readiness("/missing").await;
}

/// STATEMENT: An initialized resource fallback gates HTTP/2 dispatch even when never selected.
///
/// HOW:
/// - Match a resource's `GET` route to initialize its fallback without invoking it.
/// - Mark that fallback unavailable and send two requests to an unrelated healthy route.
/// - Observe pending readiness, no responses, and no application entry until recovery is notified.
/// - Verify both queued requests complete without rebinding the fallback.
#[ntex::test]
async fn test_h2_initialized_resource_fallback_readiness_blocks_dispatch() {
    h2_initialized_handler_readiness("/resource").await;
}

/// STATEMENT: HTTP/2 keeps delivering request payloads while the application is at capacity.
///
/// HOW:
/// - Limit the request service to one in-flight call and wait for it to start reading a body.
/// - Send the body after the service consumes its slot and verify the handler receives it.
#[ntex::test]
async fn test_h2_request_readiness_does_not_block_payload() {
    use ntex::http::HeaderMap;
    use ntex::util::BytesMut;
    use ntex_h2::client::SimpleClient;
    use ntex_util::services::inflight::InFlight;

    let started = Arc::new(AtomicBool::new(false));
    let received = Arc::new(Mutex::new(None));
    let started2 = started.clone();
    let received2 = received.clone();
    let srv = test_server(async move |_| {
        let started = started2.clone();
        let received = received2.clone();
        HttpService::h2(
            ntex::factory(move |mut req: Request| {
                let started = started.clone();
                let received = received.clone();
                async move {
                    started.store(true, Ordering::Release);
                    let mut body = BytesMut::new();
                    while let Some(chunk) = req.payload().next().await {
                        body.extend_from_slice(&chunk.map_err(io::Error::other)?);
                    }
                    *received.lock().unwrap() = Some(body.freeze());
                    Ok::<_, io::Error>(Response::Ok().build())
                }
            })
            .apply(InFlight::new(1)),
        )
    });

    let io = ntex::connect::connect(srv.addr()).await.unwrap();
    let client = SimpleClient::new(io, false, "localhost".into());
    let (snd, _rcv) = client
        .send(Method::POST, "/".into(), HeaderMap::default(), false)
        .await
        .unwrap();
    wait_for(|| started.load(Ordering::Acquire)).await.unwrap();
    snd.send_payload(Bytes::from_static(b"pay"), false)
        .await
        .unwrap();
    snd.send_payload(Bytes::from_static(b"load"), true)
        .await
        .unwrap();
    wait_for(|| received.lock().unwrap().is_some())
        .await
        .expect("application readiness blocked delivery of the active request's body");
    assert_eq!(
        received.lock().unwrap().take(),
        Some(Bytes::from_static(b"payload"))
    );
}

/// STATEMENT: Dropping an unfinished HTTP/2 request body after its response resets the stream.
///
/// HOW:
/// - Retain the request payload in a task, read a chunk, then drop it after a delay.
/// - Verify the response completes while the request body remains open.
/// - Attempt further payload writes and verify the request stream becomes unwritable.
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

/// STATEMENT: An HTTP/2 payload reports its stream reset once, then ends without hanging.
///
/// HOW:
/// - Keep reading the payload after the handler returns, send one chunk, and reset with `CANCEL`.
/// - Verify the reader finishes with exactly the chunk and the `CANCEL` error, without a timeout.
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

/// STATEMENT: A malformed HTTP/2 request URI is rejected without closing the connection.
///
/// HOW:
/// - Send a URI containing a space with both closed and open request bodies.
/// - Verify each receives a final `400` response and the connection remains open.
/// - Send a valid request on the same connection and verify `200 OK`.
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

/// STATEMENT: An HTTP/2 response body error resets only the failing stream.
///
/// HOW:
/// - Start a delayed successful request and a request whose response stream returns an error.
/// - Verify the failing stream sends `200` headers followed by an `INTERNAL_ERROR` reset.
/// - Verify the other response completes with its expected body and the connection stays open.
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

/// STATEMENT: HTTP/2 `304 Not Modified` responses omit bodies and content-length headers.
///
/// HOW:
/// - Return `304` responses with empty, fixed, and streaming application bodies.
/// - Verify each response ends at its headers and contains no `Content-Length`.
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

/// STATEMENT: HTTP/2 sized and unsized response bodies use their expected end-stream framing.
///
/// HOW:
/// - Return the same two chunks through a sized stream and an unsized stream.
/// - Verify successful headers with `Date`, and length six for the sized response.
/// - Verify the sized stream ends on its last chunk, while the unsized stream ends with empty data.
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

/// STATEMENT: HTTP/2 skips empty non-final data frames and resets their count after non-empty data.
///
/// HOW:
/// - Send two groups of nine empty frames, separated and followed by non-empty payload data.
/// - Verify the handler receives only `a` and `bc`, with no empty chunks.
/// - Verify the connection remains open and no `GOAWAY` is sent.
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

/// STATEMENT: HTTP/2 exposes request trailers after the payload reaches EOF.
///
/// HOW:
/// - Send raw frames carrying `abc` followed by final headers containing `x-trailer: 1`.
/// - Drain the payload and verify the body plus exactly one trailer with the expected value.
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

/// STATEMENT: Ten consecutive empty non-final HTTP/2 data frames trigger connection closure.
///
/// HOW:
/// - Send ten empty frames followed by non-empty final data.
/// - Verify `GOAWAY` carries `ENHANCE_YOUR_CALM` and the connection closes.
/// - Verify the trailing data is not delivered as a successfully completed request body.
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

/// STATEMENT: HTTP/2 expectation control can permit a body or send a final rejection.
///
/// HOW:
/// - Send `Expect: 100-Continue` and verify non-final `100` headers before transmitting the body.
/// - Verify the accepted request returns `200` and echoes the body through the final data frame.
/// - Reject another request and verify a final `417` without `100`, leaving the connection open.
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

/// STATEMENT: HTTP/2 expectation-control failure resets its stream without losing the connection.
///
/// HOW:
/// - Return a control-service error for an `Expect: 100-Continue` request.
/// - Verify an `INTERNAL_ERROR` stream reset, then successfully send a `GET` on the same connection.
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

/// STATEMENT: HTTP/2 replaces informational application responses with an internal server error.
///
/// HOW:
/// - Return `100 Continue` and `101 Switching Protocols` as application responses.
/// - Verify both requests receive `500 Internal Server Error` headers.
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

/// STATEMENT: HTTP/1 rejects request lines exceeding its size limit while accepting shorter ones.
///
/// HOW:
/// - Send a request with a path containing 16 KiB of repeated bytes and verify status `414`.
/// - Shorten the path by twenty bytes, include a host header, and verify status `200`.
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

/// STATEMENT: HTTP/1 distinguishes unsupported transfer codings from malformed coding order.
///
/// HOW:
/// - Send `Transfer-Encoding: gzip, chunked` and verify status `501`.
/// - Send `Transfer-Encoding: chunked, gzip` and verify status `400`.
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

/// STATEMENT: HTTP/1 exposes chunked request trailers without leaking them to a pipelined request.
///
/// HOW:
/// - Pipeline two chunked requests, the first with `x-trailer: 1` and the second without trailers.
/// - Drain each payload and return its body and trailers in the response.
/// - Verify the responses contain `abc|x-trailer=1` and `de|`, respectively.
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

/// STATEMENT: An HTTP/1 peer reset stops polling an always-ready response body.
///
/// HOW:
/// - Serve a body that counts polls and yields data until a large safety limit.
/// - Read part of the response, then close with unread data to reset the connection.
/// - Verify the poll count stabilizes below the safety limit and remains unchanged after a delay.
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
