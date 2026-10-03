//! Temporary h1/io benchmark.
//!
//! cargo +1.97.1 test -q --release -p ntex --test bench_h1 bench_h1 -- --exact --ignored --nocapture
//!
//! TLS=rustls|openssl|schannel runs every mode over TLS. rustls needs
//! `--features ntex/rustls`, openssl needs `--features ntex/openssl`, schannel
//! is windows only. The client always uses rustls. The std server uses openssl
//! for TLS=openssl and rustls otherwise (there is no blocking schannel server).
//!
//! Modes:
//! * default: runs an ntex server and a blocking std server in-process and
//!   benchmarks both over loopback.
//! * LISTEN=<addr>: runs only the ntex server on `addr` until killed.
//! * LISTEN_STD=<addr>: runs only the std server on `addr` until killed.
//! * SERVER_ADDR=<addr>: runs only the client against a remote server started
//!   with LISTEN or LISTEN_STD. Server cpu and allocations are reported by the
//!   server itself through `/stats`.
//!
//! env: N (requests per pipelined run, default 100000, sequential runs use N/4),
//! ONLY=<name substring>, SLEEP_US=<client sleep after each batch write>.
//! Server env: WBUF=<write buffer high watermark>, WTHR=<eager write threshold>,
//! STD_PER_RESP=1 (std server writes every response separately),
//! TRACE=<count> prints backtraces of the first allocations made by the server
//! thread during measured runs (local mode only).
//! CTR=1 prints io counters, needs files/bench-ctr.patch and `ctr()` returning
//! `ntex::io::bench_ctr::get()`.
#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::all,
    unreachable_pub,
    warnings
)]
use std::alloc::{GlobalAlloc, Layout, System as SysAlloc};
use std::cell::Cell;
use std::io::{Read, Write};
use std::sync::{Arc, atomic::AtomicU64, atomic::AtomicUsize, atomic::Ordering};
use std::{net, thread, time::Duration, time::Instant};

use ntex::http::{HttpService, Request, Response, test};
use ntex::{SharedCfg, util::Bytes};

mod rustls_utils;

// ---- counting allocator, server thread only ----

struct Counting;

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}
static TRACE_LEFT: AtomicUsize = AtomicUsize::new(0);

fn record(size: usize) {
    let _ = TRACK.try_with(|t| {
        if t.get() {
            ALLOCS.with(|a| a.set(a.get() + 1));
            BYTES.with(|b| b.set(b.get() + size as u64));
            if TRACE_LEFT.load(Ordering::Relaxed) > 0 {
                TRACE_LEFT.fetch_sub(1, Ordering::Relaxed);
                t.set(false);
                let bt = std::backtrace::Backtrace::force_capture();
                eprintln!("---- alloc {size} B\n{bt}");
                t.set(true);
            }
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { SysAlloc.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { SysAlloc.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        record(new);
        unsafe { SysAlloc.realloc(ptr, layout, new) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { SysAlloc.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn untracked<R>(f: impl FnOnce() -> R) -> R {
    let prev = TRACK.with(|t| t.replace(false));
    let r = f();
    TRACK.with(|t| t.set(prev));
    r
}

// ---- per-thread cpu time ----

#[cfg(windows)]
mod cpu {
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
    }

    /// Cycles consumed by the current thread.
    pub fn now() -> u64 {
        let mut c = 0;
        unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
        c
    }

    /// Cycles per nanosecond, measured with a busy loop.
    pub fn calibrate() -> f64 {
        let (c0, t0) = (now(), std::time::Instant::now());
        while t0.elapsed() < std::time::Duration::from_millis(200) {
            std::hint::spin_loop();
        }
        (now() - c0) as f64 / t0.elapsed().as_nanos() as f64
    }
}

#[cfg(target_os = "linux")]
mod cpu {
    /// Nanoseconds on cpu of the current thread.
    pub fn now() -> u64 {
        super::untracked(|| {
            let s = std::fs::read_to_string("/proc/thread-self/schedstat").unwrap();
            s.split_whitespace().next().unwrap().parse().unwrap()
        })
    }

    pub fn calibrate() -> f64 {
        1.0
    }
}

/// `cpu::now()` units per nanosecond, as f64 bits.
static CPN: AtomicU64 = AtomicU64::new(0);

#[cfg(any(windows, target_os = "linux"))]
fn calibrate() {
    let cpn = cpu::calibrate();
    CPN.store(cpn.to_bits(), Ordering::Relaxed);
    println!("cpu: {cpn:.3} cycles/ns");
}

#[cfg(not(any(windows, target_os = "linux")))]
fn calibrate() {}

#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    cpu_ns: u64,
    allocs: u64,
    bytes: u64,
}

impl Stats {
    /// Marks the current thread as the measured server thread and returns
    /// its counters.
    fn current() -> Stats {
        TRACK.with(|t| t.set(true));
        #[cfg(any(windows, target_os = "linux"))]
        let cpn = f64::from_bits(CPN.load(Ordering::Relaxed));
        Stats {
            #[cfg(any(windows, target_os = "linux"))]
            cpu_ns: (cpu::now() as f64 / cpn) as u64,
            #[cfg(not(any(windows, target_os = "linux")))]
            cpu_ns: 1,
            allocs: ALLOCS.with(Cell::get),
            bytes: BYTES.with(Cell::get),
        }
    }

    fn encode(&self) -> String {
        untracked(|| format!("{} {} {}", self.cpu_ns, self.allocs, self.bytes))
    }

    fn decode(s: &str) -> Stats {
        let mut it = s.split_whitespace().map(|v| v.parse::<u64>().unwrap());
        Stats {
            cpu_ns: it.next().unwrap(),
            allocs: it.next().unwrap(),
            bytes: it.next().unwrap(),
        }
    }
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---- responses ----

static SMALL: &[u8] = b"Hello world!";
static LARGE: [u8; 16 * 1024] = [b'x'; 16 * 1024];

fn body_for(path: &str) -> Bytes {
    if path == "/large" {
        Bytes::from_static(&LARGE)
    } else {
        Bytes::from_static(SMALL)
    }
}

// ---- ntex server ----

async fn handle(req: Request) -> Result<Response, std::io::Error> {
    if req.path() == "/stats" {
        Ok(Response::Ok().body(Stats::current().encode()))
    } else {
        Ok(Response::Ok().body(body_for(req.path())))
    }
}

fn server_cfg() -> SharedCfg {
    let mut io = ntex::io::IoConfig::new();
    if let Some(w) = std::env::var("WBUF").ok().and_then(|v| v.parse().ok()) {
        io = io.set_write_buf(w);
    }
    if let Some(w) = std::env::var("WTHR").ok().and_then(|v| v.parse().ok()) {
        io = io.set_write_buf_threshold(w);
    }
    SharedCfg::new("BENCH").add(io).into()
}

// ---- tls ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tls {
    None,
    Rustls,
    Openssl,
    Schannel,
}

fn tls_mode() -> Tls {
    let tls = match std::env::var("TLS").as_deref() {
        Err(_) | Ok("" | "none") => Tls::None,
        Ok("rustls") => Tls::Rustls,
        Ok("openssl") => Tls::Openssl,
        Ok("schannel") => Tls::Schannel,
        Ok(v) => panic!("TLS={v}: expected rustls, openssl or schannel"),
    };
    let supported = match tls {
        Tls::None => true,
        Tls::Rustls => cfg!(feature = "rustls"),
        Tls::Openssl => cfg!(feature = "openssl"),
        Tls::Schannel => cfg!(windows),
    };
    assert!(
        supported,
        "TLS={tls:?} is not supported, rustls and openssl need --features ntex/<name>, schannel needs windows"
    );
    tls
}

#[cfg(feature = "openssl")]
fn ssl_acceptor() -> tls_openssl::ssl::SslAcceptor {
    use tls_openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

    // v5 supports TLS 1.3, the only version of the rustls client
    let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    builder.build()
}

#[cfg(windows)]
fn schannel<F, S, St>(
    service: impl ntex::service::IntoService<
        S,
        St,
        ntex::io::Io<ntex::io::Layer<ntex_tls::schannel::SchannelFilter, F>>,
    >,
) -> impl ntex::service::Service<
    St,
    ntex::io::Io<F>,
    Res = S::Res,
    Error = ntex::server::TlsError<S::Error>,
>
where
    F: ntex::io::Filter,
    S: ntex::service::Service<
            St,
            ntex::io::Io<ntex::io::Layer<ntex_tls::schannel::SchannelFilter, F>>,
        >,
{
    use ntex::{server::TlsError, service::IntoService, service::Service};
    use ntex_tls::schannel::{Certificate, ServerConfig, TlsAcceptor};

    // `cert.pem` and `key.pem`
    let cert = Certificate::from_pkcs12(include_bytes!("identity.pfx"), "ntex").unwrap();
    let config = ServerConfig::new(cert)
        .unwrap()
        .set_alpn_protocols(ntex::http::ALPN_PROTO_H1);
    TlsAcceptor::new(config)
        .map_err(TlsError::Tls)
        .and_then(service.into_service().map_err(TlsError::Service))
}

/// Binds `$mk`, a closure creating the h1 service for `$tls`, and evaluates `$body`.
macro_rules! with_service {
    ($tls:expr, $mk:ident => $body:expr) => {
        match $tls {
            Tls::None => {
                let $mk = || HttpService::h1(handle);
                $body
            }
            #[cfg(feature = "rustls")]
            Tls::Rustls => {
                let $mk = || {
                    ntex::http::rustls(
                        rustls_utils::tls_acceptor(),
                        ntex::http::ALPN_PROTO_H1,
                        HttpService::h1(handle),
                    )
                };
                $body
            }
            #[cfg(feature = "openssl")]
            Tls::Openssl => {
                let $mk = || ntex::http::openssl(ssl_acceptor(), HttpService::h1(handle));
                $body
            }
            #[cfg(windows)]
            Tls::Schannel => {
                let $mk = || schannel(HttpService::h1(handle));
                $body
            }
            #[allow(unreachable_patterns)]
            tls => unreachable!("{tls:?}"),
        }
    };
}

fn ntex_server(tls: Tls) -> test::TestServer {
    with_service!(tls, mk => test::server_with_config(async move |_| mk(), server_cfg()))
}

async fn ntex_listen(tls: Tls, addr: net::SocketAddr) {
    with_service!(tls, mk => ntex::server::build()
        .bind("bench", addr, server_cfg(), async move |_| mk())
        .unwrap()
        .workers(1)
        .run()
        .await
        .unwrap())
}

// ---- std server ----

fn std_response(body: &[u8]) -> Vec<u8> {
    let mut res = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\ndate: Thu, 01 Jan 2026 00:00:00 GMT\r\n\r\n",
        body.len()
    )
    .into_bytes();
    res.extend_from_slice(body);
    res
}

/// Blocking std server, the floor for the same request stream.
fn std_server(listener: net::TcpListener, tls: Tls) {
    let per_resp = env("STD_PER_RESP", 0) != 0;
    #[cfg(feature = "openssl")]
    let openssl = (tls == Tls::Openssl).then(ssl_acceptor);
    let rustls = Arc::new(rustls_utils::tls_acceptor());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        stream.set_nodelay(true).unwrap();
        #[cfg(feature = "openssl")]
        if let Some(acceptor) = openssl.clone() {
            thread::spawn(move || {
                if let Ok(stream) = acceptor.accept(stream) {
                    std_conn(stream, per_resp);
                }
            });
            continue;
        }
        if tls == Tls::None {
            thread::spawn(move || std_conn(stream, per_resp));
        } else {
            let conn = tls_rustls::ServerConnection::new(rustls.clone()).unwrap();
            thread::spawn(move || std_conn(tls_rustls::StreamOwned::new(conn, stream), per_resp));
        }
    }
}

fn std_conn(mut stream: impl Read + Write, per_resp: bool) {
    let small = std_response(SMALL);
    let large = std_response(&LARGE);
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::with_capacity(2 * 1024 * 1024);
    let mut pending = 0usize;
    loop {
        let r = match stream.read(&mut buf[pending..]) {
            Ok(0) | Err(_) => return,
            Ok(r) => r,
        };
        let end = pending + r;
        let mut pos = 0;
        while let Some(i) = buf[pos..end].windows(4).position(|w| w == b"\r\n\r\n") {
            let path = buf[pos..pos + i]
                .split(|b| *b == b' ')
                .nth(1)
                .unwrap_or_default();
            if path == b"/stats" {
                let body = Stats::current().encode();
                out.extend_from_slice(&untracked(|| std_response(body.as_bytes())));
            } else if path == b"/large" {
                out.extend_from_slice(&large);
            } else {
                out.extend_from_slice(&small);
            }
            pos += i + 4;
            if per_resp && stream.write_all(&out).is_err() {
                return;
            }
            if per_resp {
                out.clear();
            }
        }
        buf.copy_within(pos..end, 0);
        pending = end - pos;
        if stream.write_all(&out).is_err() {
            return;
        }
        out.clear();
    }
}

// ---- blocking client ----

trait Conn: Read + Write {}

impl<T: Read + Write> Conn for T {}

/// Connects to `addr`, over rustls unless `tls` is `Tls::None`.
fn connect(addr: net::SocketAddr, tls: Tls) -> Box<dyn Conn> {
    let stream = net::TcpStream::connect(addr).unwrap();
    stream.set_nodelay(true).unwrap();
    if tls == Tls::None {
        return Box::new(stream);
    }
    let mut cfg = rustls_utils::tls_connector();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    let conn =
        tls_rustls::ClientConnection::new(Arc::new(cfg), "localhost".try_into().unwrap()).unwrap();
    Box::new(tls_rustls::StreamOwned::new(conn, stream))
}

fn request(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nuser-agent: bench\r\n\r\n")
}

/// Reads one response, returns its size and body.
fn read_response(stream: &mut dyn Conn) -> (usize, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..pos])
                .unwrap()
                .to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap();
            let total = pos + 4 + len;
            while buf.len() < total {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0, "connection closed");
                buf.extend_from_slice(&chunk[..n]);
            }
            assert_eq!(buf.len(), total, "unexpected trailing data");
            return (total, buf[pos + 4..].to_vec());
        }
    }
}

/// Sends `n` requests in batches of `depth`, returns the wall time.
fn run(stream: &mut dyn Conn, path: &str, depth: usize, n: usize, size: usize) -> Duration {
    let batch = request(path).repeat(depth);
    let mut chunk = vec![0u8; 1024 * 1024];
    let sleep = env("SLEEP_US", 0);
    let start = Instant::now();
    let mut sent = 0;
    while sent < n {
        stream.write_all(batch.as_bytes()).unwrap();
        if sleep > 0 {
            thread::sleep(Duration::from_micros(sleep as u64));
        }
        let mut need = depth * size;
        while need > 0 {
            let r = stream.read(&mut chunk).unwrap();
            assert!(r > 0 && r <= need, "read {r} need {need}");
            need -= r;
        }
        sent += depth;
    }
    start.elapsed()
}

fn server_stats(stream: &mut dyn Conn) -> Stats {
    stream.write_all(request("/stats").as_bytes()).unwrap();
    Stats::decode(std::str::from_utf8(&read_response(stream).1).unwrap())
}

fn report(name: &str, n: usize, wall: Duration, s: Stats) {
    let n = n as f64;
    println!(
        "{name:<18} {:>8} req {:>9.2} us/req wall {:>9.3} us/req cpu {:>6.2} allocs/req {:>8.0} B/req",
        n,
        wall.as_secs_f64() * 1e6 / n,
        s.cpu_ns as f64 / 1000.0 / n,
        s.allocs as f64 / n,
        s.bytes as f64 / n,
    );
}

fn delta(a: Stats, b: Stats) -> Stats {
    Stats {
        cpu_ns: b.cpu_ns - a.cpu_ns,
        allocs: b.allocs - a.allocs,
        bytes: b.bytes - a.bytes,
    }
}

/// Io counters, needs files/bench-ctr.patch: return
/// `ntex::io::bench_ctr::get()` and set CTR=1.
fn ctr() -> [u64; 20] {
    [0; 20]
}

fn print_ctr(n: usize, a: [u64; 20], b: [u64; 20]) {
    const NAMES: [&str; 15] = [
        "recv",
        "recvB",
        "wsarecv0",
        "wsarecv0Pend",
        "wsasend",
        "sendB",
        "sendSync",
        "sendPend",
        "sendDoneB",
        "gqcs",
        "h1poll",
        "wbSet",
        "rdWbPause",
        "sendBufs",
        "flushPend",
    ];
    let n = n as f64;
    let s: Vec<String> = NAMES
        .iter()
        .enumerate()
        .map(|(i, nm)| format!("{nm}={:.3}", (b[i] - a[i]) as f64 / n))
        .collect();
    println!("    per req: {}", s.join(" "));
}

fn cases() -> Vec<(String, &'static str, usize)> {
    let only = std::env::var("ONLY").unwrap_or_default();
    let mut v = Vec::new();
    for (body, path) in [("small", "/small"), ("16k", "/large")] {
        for depth in [1usize, 16, 64] {
            let name = if depth == 1 {
                format!("{body} seq")
            } else {
                format!("{body} pipe{depth}")
            };
            if name.contains(&only) {
                v.push((name, path, depth));
            }
        }
    }
    v
}

/// Runs every case against `addr`, a new connection per case.
fn run_client(prefix: &str, addr: net::SocketAddr, tls: Tls, trace: usize) {
    let n = env("N", 100_000);
    let show_ctr = std::env::var("CTR").is_ok();
    for (name, path, depth) in cases() {
        let n = if depth == 1 { n / 4 } else { n } / depth * depth;
        let mut conn = connect(addr, tls);
        let stream = &mut *conn;

        stream.write_all(request(path).as_bytes()).unwrap();
        let (size, _) = read_response(stream);
        // warm up
        run(stream, path, depth, (n / 10).max(depth), size);

        let s0 = server_stats(stream);
        TRACE_LEFT.store(trace, Ordering::Relaxed);
        let c0 = ctr();
        let wall = run(stream, path, depth, n, size);
        let c1 = ctr();
        TRACE_LEFT.store(0, Ordering::Relaxed);
        let s1 = server_stats(stream);
        report(&format!("{prefix}{name}"), n, wall, delta(s0, s1));
        if show_ctr {
            print_ctr(n, c0, c1);
        }
    }
}

fn addr_env(name: &str) -> Option<net::SocketAddr> {
    std::env::var(name).ok().map(|v| {
        use std::net::ToSocketAddrs;
        v.to_socket_addrs()
            .unwrap_or_else(|e| panic!("{name}={v}: {e}"))
            .next()
            .unwrap()
    })
}

#[ntex::test]
#[ignore]
async fn bench_h1() {
    log::set_max_level(log::LevelFilter::Info);
    let tls = tls_mode();

    // remote client
    if let Some(addr) = addr_env("SERVER_ADDR") {
        println!(
            "client: server {addr}, tls {tls:?}, N={}",
            env("N", 100_000)
        );
        thread::spawn(move || run_client("", addr, tls, 0))
            .join()
            .unwrap();
        return;
    }

    // remote std server
    if let Some(addr) = addr_env("LISTEN_STD") {
        calibrate();
        let listener = net::TcpListener::bind(addr).unwrap();
        println!(
            "std server listening on {}, tls {tls:?}",
            listener.local_addr().unwrap()
        );
        thread::spawn(move || std_server(listener, tls))
            .join()
            .unwrap();
        return;
    }

    // remote ntex server
    if let Some(addr) = addr_env("LISTEN") {
        calibrate();
        println!("ntex server listening on {addr}, tls {tls:?}");
        ntex_listen(tls, addr).await;
        return;
    }

    // local loopback
    calibrate();
    println!("N={}, tls {tls:?}", env("N", 100_000));
    let trace = env("TRACE", 0);
    let srv = ntex_server(tls);
    let addr = srv.addr();

    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let std_addr = listener.local_addr().unwrap();
    thread::spawn(move || std_server(listener, tls));

    thread::spawn(move || {
        run_client("", addr, tls, trace);
        run_client("std ", std_addr, tls, 0);
    })
    .join()
    .unwrap();
}
