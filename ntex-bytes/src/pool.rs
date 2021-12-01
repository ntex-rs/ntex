use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::Release};

use ntex_util::task::LocalWaker;
use slab::Slab;

use crate::BytesMut;

#[derive(Clone)]
pub struct Pool {
    idx: usize,
    inner: &'static LocalPool,
}

#[derive(Copy, Clone)]
pub struct PoolRef(&'static LocalPool);

struct MemoryPool {
    item: PoolItem,
    local: &'static LocalPool,
}

struct LocalPool {
    item: PoolItem,
    _slab: Slab<LocalWaker>,
}

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
    pub fn pool(&self) -> PoolRef {
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
    pub fn id(&self) -> PoolId {
        self.inner.item.0.id
    }

    #[inline]
    pub fn buf_with_capacity(&self, cap: usize) -> crate::BytesMut {
        crate::BytesMut::with_capacity_in_priv(cap, self.inner.item)
    }

    #[inline]
    pub fn allocated(&self) -> usize {
        self.inner.item.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(&self, buf: &mut crate::BytesMut) {
        buf.move_to_pool(self.inner.item);
    }
}

//impl Default for Pool {
//    fn default() -> Pool {
//        POOLS.with(|pools| pools[PoolId::DEFAULT.0 as usize])
//    }
//}

impl PoolRef {
    #[inline]
    pub fn id(&self) -> PoolId {
        self.0.item.0.id
    }

    #[inline]
    pub fn pool(&self) -> Pool {
        Pool {
            idx: 0,
            inner: self.0,
        }
    }

    #[inline]
    pub fn allocated(&self) -> usize {
        self.0.item.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(&self, buf: &mut BytesMut) {
        buf.move_to_pool(self.0.item);
    }

    #[inline]
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        BytesMut::with_capacity_in_priv(cap, self.0.item)
    }

    #[inline]
    pub(super) fn item(&self) -> PoolItem {
        self.0.item
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
        }));

        MemoryPool { item, local }
    }
}
