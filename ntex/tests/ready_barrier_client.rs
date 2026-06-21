//! Diagnostic repro for ntex-rs/ntex#911.
//!
//! The server waits for `ServerStatus::Ready` before issuing the first client
//! request. On GitHub Actions this can still fail under the default runtime path.

use std::net;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use ntex::client::Client;
use ntex::http::{HttpService, HttpServiceConfig, StatusCode};
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

async fn one_round(name: &'static str) {
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));
    let app_name = format!("{name}-ready-barrier");

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
            .listen("ready-barrier", listener, async |_| {
                HttpService::new(App::new().service(
                    web::resource("/").to(|| async { HttpResponse::Ok().body("ok") }),
                ))
            })
            .unwrap()
            .config(
                "ready-barrier",
                SharedCfg::new("READY-BARRIER-SRV")
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

    let url = format!("http://{addr}/");
    eprintln!("{name}: sending first request to {url}");

    let client = Client::new().await;
    let response = client.get(url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = response.body().await.unwrap();
    assert_eq!(body.as_ref(), b"ok");

    server.stop(false).await;
}

macro_rules! define_ready_barrier_probe {
    ($($name:ident),+ $(,)?) => {
        $(
            #[ntex::test]
            async fn $name() {
                one_round(stringify!($name)).await;
            }
        )+
    };
}

define_ready_barrier_probe!(
    ready_barrier_client_00,
    ready_barrier_client_01,
    ready_barrier_client_02,
    ready_barrier_client_03,
    ready_barrier_client_04,
    ready_barrier_client_05,
    ready_barrier_client_06,
    ready_barrier_client_07,
    ready_barrier_client_08,
    ready_barrier_client_09,
    ready_barrier_client_10,
    ready_barrier_client_11,
    ready_barrier_client_12,
    ready_barrier_client_13,
    ready_barrier_client_14,
    ready_barrier_client_15,
);
