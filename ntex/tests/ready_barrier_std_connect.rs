//! Diagnostic repro for ntex-rs/ntex#911 using a blocking std TCP client.
//!
//! This removes ntex's async client connect path from the first request. The
//! server still waits for `ServerStatus::Ready` before the connection attempt.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{self, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ntex::http::{HttpService, HttpServiceConfig};
use ntex::io::IoConfig;
use ntex::server::{self, ServerStatus};
use ntex::web::{self, App, HttpResponse, WebAppConfig};
use ntex::{SharedCfg, rt};

async fn wait_for_ready(name: &'static str, rx: mpsc::Receiver<()>) {
    let timeout = std::env::var("READY_TIMEOUT_SECS")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(5));
    let started = Instant::now();
    let result = rt::spawn_blocking(move || rx.recv_timeout(timeout)).await;

    match result {
        Ok(Ok(())) => eprintln!(
            "{name}: observed ServerStatus::Ready after {:?}",
            started.elapsed()
        ),
        Ok(Err(err)) => {
            panic!(
                "{name}: timed out after {:?} waiting for ServerStatus::Ready: {err:?}",
                started.elapsed()
            )
        }
        Err(err) => panic!("{name}: spawn_blocking failed while waiting for Ready: {err}"),
    }
}

async fn std_http_get(name: &'static str, addr: net::SocketAddr) -> (String, Vec<u8>) {
    let result = rt::spawn_blocking(move || {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;

        let request =
            format!("GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes())?;

        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status)?;

        let mut content_len = 0usize;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let trimmed = header.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_len = value.trim().parse().unwrap_or(0);
            }
        }

        let mut body = vec![0; content_len];
        reader.read_exact(&mut body)?;

        Ok::<_, std::io::Error>((status, body))
    })
    .await;

    match result {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => panic!("{name}: std TcpStream request failed: {err:?}"),
        Err(err) => panic!("{name}: spawn_blocking failed during std request: {err}"),
    }
}

async fn one_round(name: &'static str) {
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
    let app_name = format!("{name}-std-connect");

    #[cfg(unix)]
    eprintln!(
        "{name}: listener bound at {addr} fd={}",
        listener.as_raw_fd()
    );
    #[cfg(not(unix))]
    eprintln!("{name}: listener bound at {addr}");

    let server =
        server::build()
            .workers(1)
            .disable_signals()
            .status_handler({
                let ready_tx = Arc::clone(&ready_tx);
                move |status| {
                    eprintln!("{name}: server status {status:?}");
                    if status == ServerStatus::Ready
                        && let Some(tx) = ready_tx.lock().unwrap().take()
                    {
                        let _ = tx.send(());
                    }
                }
            })
            .listen("std-connect", listener, async |_| {
                HttpService::new(App::new().service(
                    web::resource("/").to(|| async { HttpResponse::Ok().body("ok") }),
                ))
            })
            .unwrap()
            .config(
                "std-connect",
                SharedCfg::new("STD-CONNECT-SRV")
                    .add(IoConfig::new())
                    .add(HttpServiceConfig::new())
                    .add(WebAppConfig::with(
                        &app_name,
                        false,
                        addr,
                        format!("{addr}"),
                    )),
            )
            .run();

    wait_for_ready(name, ready_rx).await;

    eprintln!("{name}: sending std TcpStream request to {addr}");
    let (status, body) = std_http_get(name, addr).await;

    assert!(status.starts_with("HTTP/1.1 200 OK"), "{status:?}");
    assert_eq!(body.as_slice(), b"ok");

    server.stop(false).await;
}

macro_rules! define_std_connect_probe {
    ($($name:ident),+ $(,)?) => {
        $(
            #[ntex::test]
            async fn $name() {
                one_round(stringify!($name)).await;
            }
        )+
    };
}

define_std_connect_probe!(
    ready_barrier_std_connect_00,
    ready_barrier_std_connect_01,
    ready_barrier_std_connect_02,
    ready_barrier_std_connect_03,
    ready_barrier_std_connect_04,
    ready_barrier_std_connect_05,
    ready_barrier_std_connect_06,
    ready_barrier_std_connect_07,
    ready_barrier_std_connect_08,
    ready_barrier_std_connect_09,
    ready_barrier_std_connect_10,
    ready_barrier_std_connect_11,
    ready_barrier_std_connect_12,
    ready_barrier_std_connect_13,
    ready_barrier_std_connect_14,
    ready_barrier_std_connect_15,
);
