#![allow(warnings, dead_code)]
use std::cell::Cell;

const POOL_WIDTH: usize = 4;
const POOL_COUNT: usize = 0b10000;
const POOL_DEFAULT: usize = 0b1111;

#[cfg(target_pointer_width = "64")]
const POOL_OFFSET: usize = 64 - POOL_WIDTH;
#[cfg(target_pointer_width = "32")]
const POOL_OFFSET: usize = 32 - POOL_WIDTH;

const POOL_MASK: usize = 0b1111 << POOL_OFFSET;
const POOL_NUM_MASK: usize = !POOL_MASK;

#[derive(Copy, Clone, Debug)]
pub struct PoolId(usize);

pub const POOL_0: PoolId = PoolId(0);
pub const POOL_1: PoolId = PoolId(1 << POOL_OFFSET);
pub const POOL_2: PoolId = PoolId(2 << POOL_OFFSET);
pub const POOL_3: PoolId = PoolId(3 << POOL_OFFSET);
pub const POOL_4: PoolId = PoolId(4 << POOL_OFFSET);
pub const POOL_5: PoolId = PoolId(5 << POOL_OFFSET);
pub const POOL_6: PoolId = PoolId(6 << POOL_OFFSET);
pub const POOL_7: PoolId = PoolId(7 << POOL_OFFSET);
pub const POOL_8: PoolId = PoolId(8 << POOL_OFFSET);
pub const POOL_9: PoolId = PoolId(9 << POOL_OFFSET);
pub const POOL_10: PoolId = PoolId(10 << POOL_OFFSET);
pub const POOL_11: PoolId = PoolId(11 << POOL_OFFSET);
pub const POOL_12: PoolId = PoolId(12 << POOL_OFFSET);
pub const POOL_13: PoolId = PoolId(13 << POOL_OFFSET);
pub const POOL_14: PoolId = PoolId(14 << POOL_OFFSET);
pub const POOL_15: PoolId = PoolId(15 << POOL_OFFSET);

pub struct Pool {
    size: Cell<usize>,
}

impl PoolId {
    #[inline]
    pub(crate) const fn get(storage: usize) -> PoolId {
        PoolId(storage & POOL_MASK)
    }

    #[inline]
    pub(crate) const fn set(&self, storage: usize) -> usize {
        storage ^ self.0
    }

    #[inline]
    pub(crate) const fn get_inline(storage: usize) -> PoolId {
        PoolId((storage << POOL_OFFSET - 8) & POOL_MASK)
    }

    #[inline]
    pub(crate) const fn set_inline(&self, storage: usize) -> usize {
        (self.0 >> POOL_OFFSET - 8) ^ storage
    }

    #[inline]
    pub(super) fn pool(&self) -> &'static Pool {
        POOLS.with(|pools| pools[self.0])
    }
}

thread_local! {
    static POOLS: [&'static Pool; POOL_COUNT] = [
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
        Pool::create(),
    ];
}

impl Pool {
    #[inline]
    pub fn default() -> &'static Pool {
        POOLS.with(|pools| pools[POOL_DEFAULT])
    }

    #[inline]
    pub const fn default_id() -> PoolId {
        POOL_15
    }

    fn create() -> &'static Pool {
        let pool = Box::new(Pool { size: Cell::new(0) });
        Box::leak(pool)
    }

    pub(super) fn alloc(&self, _size: usize) {
        todo!()
    }

    pub(super) fn release(&self, _size: usize) {
        todo!()
    }
}
