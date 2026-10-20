#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::all, clippy::pedantic)]

//! Allocation paths with the `tracker` feature off, on but untracked,
//! and tracked. Without the feature only the untracked cases run.
use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
#[cfg(feature = "tracker")]
use ntex_bytes::BytesTracker;
use ntex_bytes::{BytePageSize, BytesMut};

#[derive(Clone, Copy)]
enum Mode {
    Untracked,
    #[cfg(feature = "tracker")]
    Tracked,
}

struct Alloc {
    #[cfg(feature = "tracker")]
    tracker: Option<BytesTracker>,
}

impl Alloc {
    fn new(mode: Mode) -> Self {
        match mode {
            Mode::Untracked => Alloc {
                #[cfg(feature = "tracker")]
                tracker: None,
            },
            #[cfg(feature = "tracker")]
            Mode::Tracked => Alloc {
                tracker: Some(BytesTracker::new()),
            },
        }
    }

    #[inline]
    fn page(&self) -> BytesMut {
        #[cfg(feature = "tracker")]
        if let Some(tracker) = &self.tracker {
            return BytesMut::with_page_size_in(BytePageSize::Size4, tracker);
        }
        BytesMut::with_page_size(BytePageSize::Size4)
    }

    #[inline]
    fn buf(&self) -> BytesMut {
        #[allow(unused_mut)]
        let mut buf = BytesMut::with_capacity(64);
        #[cfg(feature = "tracker")]
        if let Some(tracker) = &self.tracker {
            buf.set_tracker(tracker);
        }
        buf
    }
}

fn modes() -> Vec<(&'static str, Mode)> {
    vec![
        ("untracked", Mode::Untracked),
        #[cfg(feature = "tracker")]
        ("tracked", Mode::Tracked),
    ]
}

fn benches(c: &mut Criterion) {
    for (name, mode) in modes() {
        let alloc = Alloc::new(mode);

        // cache hit and release back to the cache
        c.bench_function(&format!("page_reuse/{name}"), |b| {
            b.iter(|| black_box(alloc.page()))
        });

        // realloc growth of a unique buffer
        c.bench_function(&format!("grow/{name}"), |b| {
            b.iter(|| {
                let mut buf = alloc.buf();
                buf.reserve(8 * 1024);
                black_box(buf)
            })
        });

        // split and freeze views of one page
        c.bench_function(&format!("split_freeze/{name}"), |b| {
            let data = [1u8; 1024];
            b.iter(|| {
                let mut buf = alloc.page();
                buf.extend_from_slice(&data);
                let head = buf.split_to(256);
                let mid = buf.split_to(256);
                black_box((head, mid, buf.freeze()))
            })
        });

        // final release on another thread, pages above the cache limit are freed
        c.bench_function(&format!("release_other_thread/{name}"), |b| {
            b.iter_custom(|iters| {
                let bufs: Vec<_> = (0..iters).map(|_| alloc.page()).collect();
                std::thread::spawn(move || {
                    let start = Instant::now();
                    drop(bufs);
                    start.elapsed()
                })
                .join()
                .unwrap_or(Duration::ZERO)
            })
        });
    }
}

criterion_group!(tracker_benches, benches);
criterion_main!(tracker_benches);
