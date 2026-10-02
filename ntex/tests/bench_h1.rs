//! Temporary h1/io benchmark, not for commit.
//!
//! cargo +1.97.1 test -q --release -p ntex --test bench_h1_tmp bench_h1 -- --exact --ignored --nocapture
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
#![allow(clippy::pedantic, clippy::nursery, clippy::all, unreachable_pub)]

use std::alloc::{GlobalAlloc, Layout, System as SysAlloc};
use std::cell::Cell;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::{net, thread, time::Duration, time::Instant};

use ntex::http::{HttpService, Request, Response, test};
use ntex::{SharedCfg, util::Bytes};

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

fn calibrate() {
    let cpn = cpu::calibrate();
    CPN.store(cpn.to_bits(), Ordering::Relaxed);
    println!("cpu: {cpn:.3} cycles/ns");
}

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
        let cpn = f64::from_bits(CPN.load(Ordering::Relaxed));
        Stats {
            cpu_ns: (cpu::now() as f64 / cpn) as u64,
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
fn std_server(listener: net::TcpListener) {
    let per_resp = env("STD_PER_RESP", 0) != 0;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        thread::spawn(move || std_conn(stream, per_resp));
    }
}

fn std_conn(mut stream: net::TcpStream, per_resp: bool) {
    let small = std_response(SMALL);
    let large = std_response(&LARGE);
    stream.set_nodelay(true).unwrap();
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
            let path = buf[pos..pos + i].split(|b| *b == b' ').nth(1).unwrap_or_default();
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

fn request(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nhost: localhost\r\nuser-agent: bench\r\n\r\n")
}

/// Reads one response, returns its size and body.
fn read_response(stream: &mut net::TcpStream) -> (usize, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "connection closed");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = std::str::from_utf8(&buf[..pos]).unwrap().to_ascii_lowercase();
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
fn run(stream: &mut net::TcpStream, path: &str, depth: usize, n: usize, size: usize) -> Duration {
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

fn server_stats(stream: &mut net::TcpStream) -> Stats {
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
fn run_client(prefix: &str, addr: net::SocketAddr, trace: usize) {
    let n = env("N", 100_000);
    let show_ctr = std::env::var("CTR").is_ok();
    for (name, path, depth) in cases() {
        let n = if depth == 1 { n / 4 } else { n } / depth * depth;
        let mut stream = net::TcpStream::connect(addr).unwrap();
        stream.set_nodelay(true).unwrap();

        stream.write_all(request(path).as_bytes()).unwrap();
        let (size, _) = read_response(&mut stream);
        // warm up
        run(&mut stream, path, depth, (n / 10).max(depth), size);

        let s0 = server_stats(&mut stream);
        TRACE_LEFT.store(trace, Ordering::Relaxed);
        let c0 = ctr();
        let wall = run(&mut stream, path, depth, n, size);
        let c1 = ctr();
        TRACE_LEFT.store(0, Ordering::Relaxed);
        let s1 = server_stats(&mut stream);
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

    // remote client
    if let Some(addr) = addr_env("SERVER_ADDR") {
        println!("client: server {addr}, N={}", env("N", 100_000));
        thread::spawn(move || run_client("", addr, 0)).join().unwrap();
        return;
    }

    // remote std server
    if let Some(addr) = addr_env("LISTEN_STD") {
        calibrate();
        let listener = net::TcpListener::bind(addr).unwrap();
        println!("std server listening on {}", listener.local_addr().unwrap());
        thread::spawn(move || std_server(listener)).join().unwrap();
        return;
    }

    // remote ntex server
    if let Some(addr) = addr_env("LISTEN") {
        calibrate();
        println!("ntex server listening on {addr}");
        ntex::server::build()
            .bind("bench", addr, server_cfg(), async |_| HttpService::h1(handle))
            .unwrap()
            .workers(1)
            .run()
            .await
            .unwrap();
        return;
    }

    // local loopback
    calibrate();
    println!("N={}", env("N", 100_000));
    let trace = env("TRACE", 0);
    let srv = test::server_with_config(async |_| HttpService::h1(handle), server_cfg());
    let addr = srv.addr();

    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let std_addr = listener.local_addr().unwrap();
    thread::spawn(move || std_server(listener));

    thread::spawn(move || {
        run_client("", addr, trace);
        run_client("std ", std_addr, 0);
    })
    .join()
    .unwrap();
}
