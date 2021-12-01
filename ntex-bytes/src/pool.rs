use std::sync::atomic::{AtomicUsize, Ordering::Relaxed, Ordering::Release};
use std::sync::Arc;

use crate::BytesMut;

#[derive(Clone)]
pub struct Pool {
    inner: PoolInner,
}

#[derive(Clone)]
pub(crate) struct PoolInner(Arc<PoolPriv>);

struct PoolPriv {
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
        POOLS.with(|pools| pools[self.0 as usize].clone())
    }

    pub(super) fn inner(&self) -> PoolInner {
        POOLS.with(|pools| pools[self.0 as usize].inner.clone())
    }

    #[inline]
    pub fn buf_with_capacity(&self, cap: usize) -> BytesMut {
        POOLS.with(|pools| pools[self.0 as usize].buf_with_capacity(cap))
    }
}

thread_local! {
    static POOLS: [&'static Pool; 16] = [
        Pool::create(PoolId::P0),
        Pool::create(PoolId::P1),
        Pool::create(PoolId::P2),
        Pool::create(PoolId::P3),
        Pool::create(PoolId::P4),
        Pool::create(PoolId::P5),
        Pool::create(PoolId::P6),
        Pool::create(PoolId::P7),
        Pool::create(PoolId::P8),
        Pool::create(PoolId::P9),
        Pool::create(PoolId::P10),
        Pool::create(PoolId::P11),
        Pool::create(PoolId::P12),
        Pool::create(PoolId::P13),
        Pool::create(PoolId::P14),
        Pool::create(PoolId::DEFAULT),
    ];
}

impl Pool {
    fn create(id: PoolId) -> &'static Pool {
        let pool = Box::new(Pool {
            inner: PoolInner(Arc::new(PoolPriv {
                id,
                size: AtomicUsize::new(0),
            })),
        });
        Box::leak(pool)
    }

    #[inline]
    pub fn id(&self) -> PoolId {
        self.inner.0.id
    }

    #[inline]
    pub fn buf_with_capacity(&self, cap: usize) -> crate::BytesMut {
        crate::BytesMut::with_capacity_in_priv(cap, self.inner())
    }

    #[inline]
    pub fn allocated(&self) -> usize {
        self.inner.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(&self, buf: &mut crate::BytesMut) {
        buf.move_to_pool(self.inner.clone());
    }

    #[inline]
    pub(super) fn inner(&self) -> PoolInner {
        self.inner.clone()
    }
}

impl Default for Pool {
    fn default() -> Pool {
        POOLS.with(|pools| pools[PoolId::DEFAULT.0 as usize].clone())
    }
}

impl PoolInner {
    #[inline]
    pub(crate) fn acquire(&self, size: usize) {
        self.0.size.fetch_add(size, Relaxed);
    }

    #[inline]
    pub(crate) fn release(&self, size: usize) {
        self.0.size.fetch_sub(size, Release);
    }
}
