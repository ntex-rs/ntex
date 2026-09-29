#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::all, clippy::pedantic)]

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use ntex_bytes::Buf;

/// Dummy Buf implementation
struct TestBuf {
    buf: &'static [u8],
    readlens: &'static [usize],
    init_pos: usize,
    pos: usize,
    readlen_pos: usize,
    readlen: usize,
}
impl TestBuf {
    fn new(buf: &'static [u8], readlens: &'static [usize], init_pos: usize) -> TestBuf {
        let mut buf = TestBuf {
            buf,
            readlens,
            init_pos,
            pos: 0,
            readlen_pos: 0,
            readlen: 0,
        };
        buf.reset();
        buf
    }
    fn reset(&mut self) {
        self.pos = self.init_pos;
        self.readlen_pos = 0;
        self.next_readlen();
    }
    /// Compute the length of the next read :
    /// - use the next value specified in readlens (capped by remaining) if any
    /// - else the remaining
    fn next_readlen(&mut self) {
        self.readlen = self.buf.len() - self.pos;
        if let Some(readlen) = self.readlens.get(self.readlen_pos) {
            self.readlen = std::cmp::min(self.readlen, *readlen);
            self.readlen_pos += 1;
        }
    }
}
impl Buf for TestBuf {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn advance(&mut self, cnt: usize) {
        self.pos += cnt;
        assert!(self.pos <= self.buf.len());
        self.next_readlen();
    }
    fn chunk(&self) -> &[u8] {
        if self.readlen == 0 {
            Default::default()
        } else {
            &self.buf[self.pos..self.pos + self.readlen]
        }
    }
}

/// Dummy Buf implementation
///  version with methods forced to not be inlined (to simulate costly calls)
struct TestBufC {
    inner: TestBuf,
}
impl TestBufC {
    fn new(buf: &'static [u8], readlens: &'static [usize], init_pos: usize) -> TestBufC {
        TestBufC {
            inner: TestBuf::new(buf, readlens, init_pos),
        }
    }
    fn reset(&mut self) {
        self.inner.reset()
    }
}
impl Buf for TestBufC {
    #[inline(never)]
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }
    #[inline(never)]
    fn advance(&mut self, cnt: usize) {
        self.inner.advance(cnt)
    }
    #[inline(never)]
    fn chunk(&self) -> &[u8] {
        self.inner.chunk()
    }
}

macro_rules! bench {
    ($c:expr, $name:expr, testbuf $testbuf:ident $readlens:expr, $method:ident $(,$arg:expr)*) => {{
        let mut bufs = [
            $testbuf::new(&[1u8; 8 + 0], $readlens, 0),
            $testbuf::new(&[1u8; 8 + 1], $readlens, 1),
            $testbuf::new(&[1u8; 8 + 2], $readlens, 2),
            $testbuf::new(&[1u8; 8 + 3], $readlens, 3),
            $testbuf::new(&[1u8; 8 + 4], $readlens, 4),
            $testbuf::new(&[1u8; 8 + 5], $readlens, 5),
            $testbuf::new(&[1u8; 8 + 6], $readlens, 6),
            $testbuf::new(&[1u8; 8 + 7], $readlens, 7),
        ];
        $c.bench_function($name, |b| {
            b.iter(|| {
                for buf in bufs.iter_mut() {
                    buf.reset();
                    let buf: &mut dyn Buf = buf; // type erasure
                    black_box(buf.$method($($arg,)*));
                }
            })
        });
    }};
    ($c:expr, $name:expr, slice, $method:ident $(,$arg:expr)*) => {{
        // buf must be long enough for one read of 8 bytes starting at pos 7
        let arr = [1u8; 8 + 7];
        $c.bench_function($name, |b| {
            b.iter(|| {
                for i in 0..8 {
                    let mut buf = &arr[i..];
                    let buf = &mut buf as &mut dyn Buf; // type erasure
                    black_box(buf.$method($($arg,)*));
                }
            })
        });
    }};
}

macro_rules! bench_group {
    ($c:expr, $group:literal, $method:ident $(,$arg:expr)*) => {
        bench!($c, concat!($group, "/slice"), slice, $method $(,$arg)*);
        bench!($c, concat!($group, "/tbuf_1"), testbuf TestBuf &[], $method $(,$arg)*);
        bench!($c, concat!($group, "/tbuf_1_costly"), testbuf TestBufC &[], $method $(,$arg)*);
        bench!($c, concat!($group, "/tbuf_2"), testbuf TestBuf &[1], $method $(,$arg)*);
        bench!($c, concat!($group, "/tbuf_2_costly"), testbuf TestBufC &[1], $method $(,$arg)*);
    };
}

fn benches(c: &mut Criterion) {
    bench_group!(c, "get_u8", get_u8);
    bench_group!(c, "get_u16", get_u16);
    bench_group!(c, "get_u32", get_u32);
    bench_group!(c, "get_u64", get_u64);
    bench_group!(c, "get_f32", get_f32);
    bench_group!(c, "get_f64", get_f64);
    bench_group!(c, "get_uint24", get_uint, 3);
}

criterion_group!(bench, benches);
criterion_main!(bench);
