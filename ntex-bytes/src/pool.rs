#![allow(clippy::type_complexity)]
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::task::{Context, Poll};
use std::{cell::Cell, cell::RefCell, fmt, future::Future, pin::Pin, ptr, rc::Rc};

use crate::{BufMut, BytesMut, BytesVec};

pub struct Pool {
    inner: &'static MemoryPool,
}

#[derive(Copy, Clone)]
pub struct PoolRef(&'static MemoryPool);

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct PoolId(u8);

#[derive(Copy, Clone, Debug)]
pub struct BufParams {
    pub high: u32,
    pub low: u32,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const SPAWNED    = 0b0000_0001;
        const INCREASED  = 0b0000_0010;
    }
}

struct MemoryPool {
    id: PoolId,
    flags: Cell<Flags>,

    size: AtomicUsize,
    max_size: Cell<usize>,

    window_h: Cell<usize>,
    window_l: Cell<usize>,
    window_idx: Cell<usize>,
    window_waiters: Cell<usize>,
    windows: Cell<[(usize, usize); 10]>,

    // io read/write cache and params
    read_wm: Cell<BufParams>,
    read_cache: RefCell<Vec<BytesVec>>,
    write_wm: Cell<BufParams>,
    write_cache: RefCell<Vec<BytesVec>>,

    spawn: RefCell<Option<Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)>>>,
}

const CACHE_SIZE: usize = 16;

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
    pub fn pool(self) -> Pool {
        POOLS.with(|pools| Pool {
            inner: pools[self.0 as usize],
        })
    }

    #[inline]
    pub fn pool_ref(self) -> PoolRef {
        POOLS.with(|pools| PoolRef(pools[self.0 as usize]))
    }

    #[inline]
    /// Set max pool size
    pub fn set_pool_size(self, size: usize) -> Self {
        self.pool_ref().set_pool_size(size);
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_read_params(self, h: u32, l: u32) -> Self {
        self.pool_ref().set_read_params(h, l);
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_write_params(self, h: u32, l: u32) -> Self {
        self.pool_ref().set_write_params(h, l);
        self
    }

    /// Set future spawn fn
    pub fn set_spawn_fn<T>(self, f: T) -> Self
    where
        T: Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        let spawn: Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)> = Rc::new(f);

        POOLS.with(move |pools| {
            *pools[self.0 as usize].spawn.borrow_mut() = Some(spawn.clone());
        });

        self
    }

    /// Set future spawn fn to all pools
    pub fn set_spawn_fn_all<T>(f: T)
    where
        T: Fn(Pin<Box<dyn Future<Output = ()>>>) + 'static,
    {
        let spawn: Rc<dyn Fn(Pin<Box<dyn Future<Output = ()>>>)> = Rc::new(f);

        POOLS.with(move |pools| {
            for pool in pools.iter().take(15) {
                *pool.spawn.borrow_mut() = Some(spawn.clone());
            }
        });
    }
}

thread_local! {
    static POOLS: [&'static MemoryPool; 16] = [
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

impl PoolRef {
    #[inline]
    /// Get pool id.
    pub fn id(self) -> PoolId {
        self.0.id
    }

    #[inline]
    /// Get `Pool` instance for this pool ref.
    pub fn pool(self) -> Pool {
        Pool { inner: self.0 }
    }

    #[inline]
    /// Get total number of allocated bytes.
    pub fn allocated(self) -> usize {
        self.0.size.load(Relaxed)
    }

    #[inline]
    pub fn move_in(self, _buf: &mut BytesMut) {}

    #[inline]
    pub fn move_vec_in(self, _buf: &mut BytesVec) {}

    #[inline]
    /// Creates a new `BytesMut` with the specified capacity.
    pub fn buf_with_capacity(self, cap: usize) -> BytesMut {
        BytesMut::with_capacity(cap)
    }

    #[inline]
    /// Creates a new `BytesVec` with the specified capacity.
    pub fn vec_with_capacity(self, cap: usize) -> BytesVec {
        BytesVec::with_capacity(cap)
    }

    #[doc(hidden)]
    #[inline]
    /// Set max pool size
    pub fn set_pool_size(self, size: usize) -> Self {
        self.0.max_size.set(size);
        self.0.window_waiters.set(0);
        self.0.window_l.set(size);
        self.0.window_h.set(usize::MAX);
        self.0.window_idx.set(0);

        let mut flags = self.0.flags.get();
        flags.insert(Flags::INCREASED);
        self.0.flags.set(flags);

        // calc windows
        let mut l = size;
        let mut h = usize::MAX;
        let mut windows: [(usize, usize); 10] = Default::default();
        windows[0] = (l, h);

        for (idx, item) in windows.iter_mut().enumerate().skip(1) {
            h = l;
            l = size - (size / 100) * idx;
            *item = (l, h);
        }
        self.0.windows.set(windows);

        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params(self) -> BufParams {
        self.0.read_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn read_params_high(self) -> usize {
        self.0.read_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_read_params(self, h: u32, l: u32) -> Self {
        assert!(l < h);
        self.0.read_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params(self) -> BufParams {
        self.0.write_wm.get()
    }

    #[doc(hidden)]
    #[inline]
    pub fn write_params_high(self) -> usize {
        self.0.write_wm.get().high as usize
    }

    #[doc(hidden)]
    #[inline]
    pub fn set_write_params(self, h: u32, l: u32) -> Self {
        assert!(l < h);
        self.0.write_wm.set(BufParams { high: h, low: l });
        self
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_read_buf(self) -> BytesVec {
        if let Some(mut buf) = self.0.read_cache.borrow_mut().pop() {
            buf.clear();
            buf
        } else {
            BytesVec::with_capacity(self.0.read_wm.get().high as usize)
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Resize read buffer
    pub fn resize_read_buf(self, buf: &mut BytesVec) {
        let (hw, lw) = self.0.write_wm.get().unpack();
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release read buffer, buf must be allocated from this pool
    pub fn release_read_buf(self, buf: BytesVec) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.read_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.read_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                v.push(buf);
            }
        }
    }

    #[doc(hidden)]
    #[inline]
    pub fn get_write_buf(self) -> BytesVec {
        if let Some(mut buf) = self.0.write_cache.borrow_mut().pop() {
            buf.clear();
            buf
        } else {
            BytesVec::with_capacity(self.0.write_wm.get().high as usize)
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Resize write buffer
    pub fn resize_write_buf(self, buf: &mut BytesVec) {
        let (hw, lw) = self.0.write_wm.get().unpack();
        let remaining = buf.remaining_mut();
        if remaining < lw {
            buf.reserve(hw - remaining);
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Release write buffer, buf must be allocated from this pool
    pub fn release_write_buf(self, buf: BytesVec) {
        let cap = buf.capacity();
        let (hw, lw) = self.0.write_wm.get().unpack();
        if cap > lw && cap <= hw {
            let v = &mut self.0.write_cache.borrow_mut();
            if v.len() < CACHE_SIZE {
                v.push(buf);
            }
        }
    }
}

impl Default for PoolRef {
    #[inline]
    fn default() -> PoolRef {
        PoolId::DEFAULT.pool_ref()
    }
}

impl From<PoolId> for PoolRef {
    #[inline]
    fn from(pid: PoolId) -> Self {
        pid.pool_ref()
    }
}

impl<'a> From<&'a Pool> for PoolRef {
    #[inline]
    fn from(pool: &'a Pool) -> Self {
        PoolRef(pool.inner)
    }
}

impl fmt::Debug for PoolRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolRef")
            .field("id", &self.id().0)
            .field("allocated", &self.allocated())
            .finish()
    }
}

impl Eq for PoolRef {}

impl PartialEq for PoolRef {
    fn eq(&self, other: &PoolRef) -> bool {
        ptr::eq(&self.0, &other.0)
    }
}

impl MemoryPool {
    fn create(id: PoolId) -> &'static MemoryPool {
        Box::leak(Box::new(MemoryPool {
            id,
            flags: Cell::new(Flags::empty()),

            size: AtomicUsize::new(0),
            max_size: Cell::new(0),

            window_h: Cell::new(0),
            window_l: Cell::new(0),
            window_waiters: Cell::new(0),
            window_idx: Cell::new(0),
            windows: Default::default(),

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
            spawn: RefCell::new(None),
        }))
    }
}

impl BufParams {
    #[inline]
    pub fn unpack(self) -> (usize, usize) {
        (self.high as usize, self.low as usize)
    }
}

impl Clone for Pool {
    #[inline]
    fn clone(&self) -> Pool {
        Pool { inner: self.inner }
    }
}

impl From<PoolId> for Pool {
    #[inline]
    fn from(pid: PoolId) -> Self {
        pid.pool()
    }
}

impl From<PoolRef> for Pool {
    #[inline]
    fn from(pref: PoolRef) -> Self {
        pref.pool()
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("id", &self.id().0)
            .field("allocated", &self.inner.size.load(Relaxed))
            .field("ready", &self.is_ready())
            .finish()
    }
}

impl Pool {
    #[inline]
    /// Get pool id.
    pub fn id(&self) -> PoolId {
        self.inner.id
    }

    #[inline]
    /// Check if pool is ready
    pub fn is_ready(&self) -> bool {
        true
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
