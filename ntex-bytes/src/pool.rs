use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::Release};
use std::task::{Context, Poll};

use ntex_util::task::LocalWaker;
use slab::Slab;

use crate::BytesMut;

pub struct Pool {
    #[allow(dead_code)]
    idx: usize,
    inner: &'static LocalPool,
}

#[derive(Copy, Clone)]
pub struct PoolRef(&'static LocalPool);

#[derive(Copy, Clone)]
pub struct BufParams {
    pub high: u16,
    pub low: u16,
}

struct MemoryPool {
    item: PoolItem,
    local: &'static LocalPool,
}

struct LocalPool {
    item: PoolItem,
    _slab: Slab<LocalWaker>,
    read_wm: Cell<BufParams>,
    read_cache: RefCell<Vec<BytesMut>>,
    write_wm: Cell<BufParams>,
    write_cache: RefCell<Vec<BytesMut>>,
}

const CACHE_SIZE: usize = 16;

#[derive(Clone, Copy)]
pub(crate) struct PoolItem(&'static PoolPrivate);

struct PoolPrivate {
    id: PoolId,
    size: AtomicUsize,
}

#[derive(Copy, Clone, Debug)]
pub struct PoolId(u8);

impl PoolId {
    pub const P0: PoolId = PoolId(0);
    pub const P1: PoolId = PoolId(1);
    pub const P2: PoolId = PoolId(2);
    pub const P3: PoolId = PoolId(3);
    pub const P4: PoolId = PoolId(4);
    pub const P5: PoolId = PoolId(5);
    pub const P6: PoolId = PoolId(6);
    pub const P7: PoolId = PoolId(7);
    pub const P8: PoolId = PoolId(8);
    pub const P9: PoolId = PoolId(9);
    pub const P10: PoolId = PoolId(10);
    pub const P11: PoolId = PoolId(11);
    pub const P12: PoolId = PoolId(12);
    pub const P13: PoolId = PoolId(13);
    pub const P14: PoolId = PoolId(14);
    pub const DEFAULT: PoolId = PoolId(15);

    #[inline]
    pub fn pool(&self) -> Pool {
        POOLS.with(|pools| Pool {
            idx: 0,
            inner: pools[self.0 as usize].local,
        })
    }

    #[inline]
    pub fn pool_ref(&self) -> PoolRef {
        POOLS.with(|pools| PoolRef(pools[self.0 as usize].local))
    }

    #[inline]
    pub(super) fn item(&self) -> PoolItem {
        POOLS.with(|pools| pools[self.0 as usize].item)
    }
}

thread_local! {
    static POOLS: [MemoryPool; 16] = [
        MemoryPool::create(PoolId::P0),
        MemoryPool::create(PoolId::P1),
        MemoryPool::create(PoolId::P2),
        MemoryPool::create(PoolId::P3),
        MemoryPool::create(PoolId::P4),
        MemoryPool::create(PoolId::P5),
        MemoryPool::create(PoolId::P6),
        MemoryPool::create(PoolId::P7),
        MemoryPool::create(PoolId::P8),
        MemoryPool::create(PoolId::P9),
        MemoryPool::create(PoolId::P10),
        MemoryPool::create(PoolId::P11),
        MemoryPool::create(PoolId::P12),
        MemoryPool::create(PoolId::P13),
        MemoryPool::create(PoolId::P14),
        MemoryPool::create(PoolId::DEFAULT),
    ];
}

impl Pool {
    #[inline]
    /// Get pool id.
    pub fn id(&self) -> PoolId {
        self.inner.item.0.id
    }

    #[inline]
    /// Get `PoolRef` instance for this pool.
    pub fn pool_ref(&self) -> PoolRef {
        PoolRef(self.inner)
    }

    #[inline]
    pub fn poll_ready(&self, _ctx: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
}

impl Clone for Pool {
    fn clone(&self) -> Pool {
        Pool {
            idx: 0,
            inner: self.inner,
        }
    }
}

impl PoolRef {
    #[inline]
    /// Get pool id.
    pub fn id(&self) -> PoolId {
        self.0.item.0.id
    }

    #[inline]
    /// Get `Pool` instance for this pool ref.
    pub fn pool(&self) -> Pool {
        Pool {
            idx: 0,
            inner: self.0,
        }
    }

    #[inline]
    /// Get total number of allocated bytes.
    pub fn allocated(&self) -> usize {
        self.0.item.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(&self, buf: &mut BytesMut) {
        buf.move_to_pool(self.0.item);
    }

    #[inline]
    /// Creates a new `BytesMut` with the specified capacity.
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity_in_priv(cap, self.0.item)
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params(&self) -> BufParams {
        self.0.read_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params_high(&self) -> usize {
        self.0.read_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_read_params(self, h: u16, l: u16) -> Self {
        assert!(l < h);
        self.0.read_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params(&self) -> BufParams {
        self.0.write_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params_high(&self) -> usize {
        self.0.write_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_write_params(self, h: u16, l: u16) -> Self {
        assert!(l < h);
        self.0.write_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_read_buf(&self) -> BytesMut {
        if let Some(buf) = self.0.read_cache.borrow_mut().pop() {
            buf
        } else {
            BytesMut::with_capacity_in_priv(
                self.0.read_wm.get().high as usize,
                self.0.item,
            )
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release read buffer, buf must be allocated from this pool
    pub fn release_read_buf(&self, mut buf: BytesMut) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.read_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.read_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                buf.clear();
                v.push(buf);
            }
        }
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_write_buf(&self) -> BytesMut {
        if let Some(buf) = self.0.write_cache.borrow_mut().pop() {
            buf
        } else {
            BytesMut::with_capacity_in_priv(
                self.0.write_wm.get().high as usize,
                self.0.item,
            )
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release write buffer, buf must be allocated from this pool
    pub fn release_write_buf(&self, mut buf: BytesMut) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.write_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.write_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                buf.clear();
                v.push(buf);
            }
        }
    }

    #[inline]
    pub(super) fn item(&self) -> PoolItem {
        self.0.item
    }
}

impl Default for PoolRef {
    #[inline]
    fn default() -> PoolRef {
        PoolId::DEFAULT.pool_ref()
    }
}

impl PoolItem {
    #[inline]
    pub(crate) fn acquire(&self, size: usize) {
        self.0.size.fetch_add(size, Relaxed);
    }

    #[inline]
    pub(crate) fn release(&self, size: usize) {
        self.0.size.fetch_sub(size, Release);
    }
}

impl MemoryPool {
    fn create(id: PoolId) -> MemoryPool {
        let item = PoolItem(Box::leak(Box::new(PoolPrivate {
            id,
            size: AtomicUsize::new(0),
        })));

        let local = Box::leak(Box::new(LocalPool {
            item,
            _slab: Slab::new(),
            read_wm: Cell::new(BufParams {
                high: 4 * 1024,
                low: 1024,
            }),
            read_cache: RefCell::new(Vec::with_capacity(CACHE_SIZE)),
            write_wm: Cell::new(BufParams {
                high: 4 * 1024,
                low: 1024,
            }),
            write_cache: RefCell::new(Vec::with_capacity(CACHE_SIZE)),
        }));

        MemoryPool { item, local }
    }
}

impl BufParams {
    #[inline]
    pub fn unpack(self) -> (usize, usize) {
        (self.high as usize, self.low as usize)
    }
}
