#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::all, clippy::pedantic)]

use std::hint::black_box;

use criterion::{Bencher, Criterion, Throughput, criterion_group, criterion_main};
use ntex_bytes::{BufMut, Bytes, BytesMut};

fn alloc_small(b: &mut Bencher<'_>) {
    b.iter(|| {
        for _ in 0..1024 {
            black_box(BytesMut::with_capacity(12));
        }
    })
}

fn alloc_mid(b: &mut Bencher<'_>) {
    b.iter(|| {
        black_box(BytesMut::with_capacity(128));
    })
}

fn alloc_big(b: &mut Bencher<'_>) {
    b.iter(|| {
        black_box(BytesMut::with_capacity(4096));
    })
}

fn split_off_and_drop(b: &mut Bencher<'_>) {
    b.iter(|| {
        for _ in 0..1024 {
            let v = vec![10; 200];
            let mut b = Bytes::from(v);
            black_box(b.split_off(100));
            black_box(b);
        }
    })
}

fn deref_unique(b: &mut Bencher<'_>) {
    let mut buf = BytesMut::with_capacity(4096);
    buf.put(&[0u8; 1024][..]);

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&buf[..]);
        }
    })
}

fn deref_unique_unroll(b: &mut Bencher<'_>) {
    let mut buf = BytesMut::with_capacity(4096);
    buf.put(&[0u8; 1024][..]);

    b.iter(|| {
        for _ in 0..128 {
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
            black_box(&buf[..]);
        }
    })
}

fn deref_shared(b: &mut Bencher<'_>) {
    let mut buf = BytesMut::with_capacity(4096);
    buf.put(&[0u8; 1024][..]);
    let buf = buf.freeze();
    let _b2 = buf.clone();

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&buf[..]);
        }
    })
}

fn deref_inline(b: &mut Bencher<'_>) {
    let mut buf = BytesMut::with_capacity(8);
    buf.put(&[0u8; 8][..]);

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&buf[..]);
        }
    })
}

fn deref_two(b: &mut Bencher<'_>) {
    let mut buf1 = BytesMut::with_capacity(8);
    buf1.put(&[0u8; 8][..]);

    let mut buf2 = BytesMut::with_capacity(4096);
    buf2.put(&[0u8; 1024][..]);

    b.iter(|| {
        for _ in 0..512 {
            black_box(&buf1[..]);
            black_box(&buf2[..]);
        }
    })
}

fn clone_inline(b: &mut Bencher<'_>) {
    let bytes = Bytes::from_static(b"hello world");

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&bytes.clone());
        }
    })
}

fn clone_static(b: &mut Bencher<'_>) {
    let bytes =
        Bytes::from_static("hello world 1234567890 and have a good byte 0987654321".as_bytes());

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&bytes.clone());
        }
    })
}

fn clone_arc(b: &mut Bencher<'_>) {
    let bytes = Bytes::from("hello world 1234567890 and have a good byte 0987654321".as_bytes());

    b.iter(|| {
        for _ in 0..1024 {
            black_box(&bytes.clone());
        }
    })
}

fn alloc_write_split_to_mid(b: &mut Bencher<'_>) {
    b.iter(|| {
        let mut buf = BytesMut::with_capacity(128);
        buf.put_slice(&[0u8; 64]);
        black_box(buf.split_to(64));
    })
}

fn drain_write_drain(b: &mut Bencher<'_>) {
    let data = [0u8; 128];

    b.iter(|| {
        let mut buf = BytesMut::with_capacity(1024);
        let mut parts = Vec::with_capacity(8);

        for _ in 0..8 {
            buf.put(&data[..]);
            parts.push(buf.split_to(128));
        }

        black_box(parts);
    })
}

fn fmt_write(b: &mut Bencher<'_>) {
    use std::fmt::Write;
    let mut buf = BytesMut::with_capacity(128);
    let s = "foo bar baz quux lorem ipsum dolor et";

    b.iter(|| {
        let _ = write!(buf, "{}", s);
        black_box(&buf);
        buf.clear();
    })
}

fn from_long_slice(b: &mut Bencher<'_>) {
    let data = [0u8; 128];
    b.iter(|| {
        let buf = BytesMut::from(&data[..]);
        black_box(buf);
    })
}

fn slice_empty(b: &mut Bencher<'_>) {
    b.iter(|| {
        let b = Bytes::from(vec![17; 1024]).clone();
        for i in 0..1000 {
            black_box(b.slice(i % 100..i % 100));
        }
    })
}

fn slice_short_from_arc(b: &mut Bencher<'_>) {
    b.iter(|| {
        // `clone` is to convert to ARC
        let b = Bytes::from(vec![17; 1024]).clone();
        for i in 0..1000 {
            black_box(b.slice(1..2 + i % 10));
        }
    })
}

// Keep in sync with storage.rs
#[cfg(target_pointer_width = "64")]
const INLINE_CAP: usize = 3 * 8 - 1;
#[cfg(target_pointer_width = "32")]
const INLINE_CAP: usize = 3 * 4 - 1;

fn slice_avg_le_inline_from_arc(b: &mut Bencher<'_>) {
    b.iter(|| {
        // `clone` is to convert to ARC
        let b = Bytes::from(vec![17; 1024]).clone();
        for i in 0..1000 {
            // [1, INLINE_CAP]
            let len = 1 + i % (INLINE_CAP - 1);
            black_box(b.slice(i % 10..i % 10 + len));
        }
    })
}

fn slice_large_le_inline_from_arc(b: &mut Bencher<'_>) {
    b.iter(|| {
        // `clone` is to convert to ARC
        let b = Bytes::from(vec![17; 1024]).clone();
        for i in 0..1000 {
            // [INLINE_CAP - 10, INLINE_CAP]
            let len = INLINE_CAP - 9 + i % 10;
            black_box(b.slice(i % 10..i % 10 + len));
        }
    })
}

fn benches(c: &mut Criterion) {
    c.bench_function("alloc_small", alloc_small);
    c.bench_function("alloc_mid", alloc_mid);
    c.bench_function("alloc_big", alloc_big);
    c.bench_function("split_off_and_drop", split_off_and_drop);
    c.bench_function("deref_unique", deref_unique);
    c.bench_function("deref_unique_unroll", deref_unique_unroll);
    c.bench_function("deref_shared", deref_shared);
    c.bench_function("deref_inline", deref_inline);
    c.bench_function("deref_two", deref_two);
    c.bench_function("clone_inline", clone_inline);
    c.bench_function("clone_static", clone_static);
    c.bench_function("clone_arc", clone_arc);
    c.bench_function("alloc_write_split_to_mid", alloc_write_split_to_mid);
    c.bench_function("drain_write_drain", drain_write_drain);
    c.benchmark_group("fmt_write")
        .throughput(Throughput::Bytes(37))
        .bench_function("fmt_write", fmt_write);
    c.benchmark_group("from_long_slice")
        .throughput(Throughput::Bytes(128))
        .bench_function("from_long_slice", from_long_slice);
    c.bench_function("slice_empty", slice_empty);
    c.bench_function("slice_short_from_arc", slice_short_from_arc);
    c.bench_function("slice_avg_le_inline_from_arc", slice_avg_le_inline_from_arc);
    c.bench_function(
        "slice_large_le_inline_from_arc",
        slice_large_le_inline_from_arc,
    );
}

criterion_group!(bench, benches);
criterion_main!(bench);
