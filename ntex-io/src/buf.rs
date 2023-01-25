use ntex_bytes::{BytesVec, PoolRef};
use ntex_util::future::Either;

use crate::IoRef;

type CacheLine = (Option<BytesVec>, Option<BytesVec>);

#[derive(Debug)]
pub struct Stack {
    len: usize,
    buffers: Either<[CacheLine; 3], Vec<CacheLine>>,
}

impl Stack {
    pub(crate) fn new() -> Self {
        Self {
            len: 1,
            buffers: Either::Left(Default::default()),
        }
    }

    pub(crate) fn add_layer(&mut self) {
        match &mut self.buffers {
            Either::Left(b) => {
                if self.len == 3 {
                    // move to vec
                    let mut vec = vec![(None, None)];
                    for item in b.iter_mut().take(self.len) {
                        vec.push((item.0.take(), item.1.take()));
                    }
                    self.len += 1;
                    self.buffers = Either::Right(vec);
                } else {
                    let mut idx = self.len;
                    while idx > 0 {
                        let item = (b[idx - 1].0.take(), b[idx - 1].1.take());
                        b[idx] = item;
                        idx -= 1;
                    }
                    b[0] = (None, None);
                    self.len += 1;
                }
            }
            Either::Right(vec) => {
                self.len += 1;
                vec.insert(0, (None, None));
            }
        }
    }

    fn get_buffers<F, R>(&mut self, idx: usize, f: F) -> R
    where
        F: FnOnce(&mut CacheLine, &mut CacheLine) -> R,
    {
        let buffers = match self.buffers {
            Either::Left(ref mut b) => &mut b[..],
            Either::Right(ref mut b) => &mut b[..],
        };

        let pos = idx + 1;
        if self.len > pos {
            let (curr, next) = buffers.split_at_mut(pos);
            f(&mut curr[idx], &mut next[0])
        } else {
            let mut curr = (buffers[idx].0.take(), None);
            let mut next = (None, buffers[idx].1.take());

            let result = f(&mut curr, &mut next);
            buffers[idx].0 = curr.0;
            buffers[idx].1 = next.1;
            result
        }
    }

    fn get_first_level(&mut self) -> &mut CacheLine {
        match &mut self.buffers {
            Either::Left(b) => &mut b[0],
            Either::Right(b) => &mut b[0],
        }
    }

    fn get_last_level(&mut self) -> &mut CacheLine {
        match &mut self.buffers {
            Either::Left(b) => &mut b[self.len - 1],
            Either::Right(b) => &mut b[self.len - 1],
        }
    }

    pub(crate) fn read_buf<F, R>(
        &mut self,
        io: &IoRef,
        idx: usize,
        nbytes: usize,
        f: F,
    ) -> R
    where
        F: FnOnce(&mut ReadBuf<'_>) -> R,
    {
        self.get_buffers(idx, |curr, next| {
            let mut buf = ReadBuf {
                io,
                nbytes,
                curr,
                next,
                need_write: false,
            };
            f(&mut buf)
        })
    }

    pub(crate) fn write_buf<F, R>(&mut self, io: &IoRef, idx: usize, f: F) -> R
    where
        F: FnOnce(&mut WriteBuf<'_>) -> R,
    {
        self.get_buffers(idx, |curr, next| {
            let mut buf = WriteBuf {
                io,
                curr,
                next,
                need_write: false,
            };
            f(&mut buf)
        })
    }

    pub(crate) fn first_read_buf_size(&mut self) -> usize {
        self.get_first_level()
            .0
            .as_ref()
            .map(|b| b.len())
            .unwrap_or(0)
    }

    pub(crate) fn first_read_buf(&mut self) -> &mut Option<BytesVec> {
        &mut self.get_first_level().0
    }

    pub(crate) fn first_write_buf(&mut self, io: &IoRef) -> &mut BytesVec {
        let item = &mut self.get_first_level().1;
        if item.is_none() {
            *item = Some(io.memory_pool().get_write_buf());
        }
        item.as_mut().unwrap()
    }

    pub(crate) fn last_read_buf(&mut self) -> &mut Option<BytesVec> {
        &mut self.get_last_level().0
    }

    pub(crate) fn last_write_buf(&mut self) -> &mut Option<BytesVec> {
        &mut self.get_last_level().1
    }

    pub(crate) fn last_write_buf_size(&mut self) -> usize {
        self.get_last_level()
            .1
            .as_ref()
            .map(|b| b.len())
            .unwrap_or(0)
    }

    pub(crate) fn set_last_write_buf(&mut self, buf: BytesVec) {
        self.get_last_level().1 = Some(buf);
    }

    pub(crate) fn release(&mut self, pool: PoolRef) {
        let items = match &mut self.buffers {
            Either::Left(b) => &mut b[..],
            Either::Right(b) => &mut b[..],
        };

        for item in items {
            if let Some(buf) = item.0.take() {
                pool.release_read_buf(buf);
            }
            if let Some(buf) = item.1.take() {
                pool.release_write_buf(buf);
            }
        }
    }

    pub(crate) fn set_memory_pool(&mut self, pool: PoolRef) {
        let items = match &mut self.buffers {
            Either::Left(b) => &mut b[..],
            Either::Right(b) => &mut b[..],
        };
        for buf in items {
            if let Some(ref mut b) = buf.0 {
                pool.move_vec_in(b);
            }
            if let Some(ref mut b) = buf.1 {
                pool.move_vec_in(b);
            }
        }
    }
}

#[derive(Debug)]
pub struct ReadBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a mut CacheLine,
    pub(crate) next: &'a mut CacheLine,
    pub(crate) nbytes: usize,
    pub(crate) need_write: bool,
}

impl<'a> ReadBuf<'a> {
    #[inline]
    /// Get number of newly added bytes
    pub fn nbytes(&self) -> usize {
        self.nbytes
    }

    #[inline]
    /// Initiate graceful io stream shutdown
    pub fn want_shutdown(&self) {
        self.io.want_shutdown()
    }

    #[inline]
    /// Get reference to source read buffer
    pub fn get_src(&mut self) -> &mut BytesVec {
        if self.next.0.is_none() {
            self.next.0 = Some(self.io.memory_pool().get_read_buf());
        }
        self.next.0.as_mut().unwrap()
    }

    #[inline]
    /// Take source read buffer
    pub fn take_src(&mut self) -> Option<BytesVec> {
        self.next
            .0
            .take()
            .and_then(|b| if b.is_empty() { None } else { Some(b) })
    }

    #[inline]
    /// Set source read buffer
    pub fn set_src(&mut self, src: Option<BytesVec>) {
        if let Some(buf) = self.next.0.take() {
            self.io.memory_pool().release_read_buf(buf);
        }
        if let Some(src) = src {
            if src.is_empty() {
                self.io.memory_pool().release_read_buf(src);
            } else {
                self.next.0 = Some(src);
            }
        }
    }

    #[inline]
    /// Get reference to destination read buffer
    pub fn get_dst(&mut self) -> &mut BytesVec {
        if self.curr.0.is_none() {
            self.curr.0 = Some(self.io.memory_pool().get_read_buf());
        }
        self.curr.0.as_mut().unwrap()
    }

    #[inline]
    /// Take destination read buffer
    pub fn take_dst(&mut self) -> BytesVec {
        self.curr
            .0
            .take()
            .unwrap_or_else(|| self.io.memory_pool().get_read_buf())
    }

    #[inline]
    /// Set destination read buffer
    pub fn set_dst(&mut self, dst: Option<BytesVec>) {
        if let Some(buf) = self.curr.0.take() {
            self.io.memory_pool().release_read_buf(buf);
        }
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_read_buf(dst);
            } else {
                self.curr.0 = Some(dst);
            }
        }
    }

    #[inline]
    /// Get reference to source and destination read buffers (src, dst)
    pub fn get_pair(&mut self) -> (&mut BytesVec, &mut BytesVec) {
        if self.next.0.is_none() {
            self.next.0 = Some(self.io.memory_pool().get_read_buf());
        }
        if self.curr.0.is_none() {
            self.curr.0 = Some(self.io.memory_pool().get_read_buf());
        }
        (self.next.0.as_mut().unwrap(), self.curr.0.as_mut().unwrap())
    }

    #[inline]
    pub fn with_write_buf<'b, F, R>(&'b mut self, f: F) -> R
    where
        F: FnOnce(&mut WriteBuf<'b>) -> R,
    {
        let mut buf = WriteBuf {
            io: self.io,
            curr: self.curr,
            next: self.next,
            need_write: self.need_write,
        };
        let result = f(&mut buf);
        self.need_write = buf.need_write;
        result
    }
}

#[derive(Debug)]
pub struct WriteBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a mut CacheLine,
    pub(crate) next: &'a mut CacheLine,
    pub(crate) need_write: bool,
}

impl<'a> WriteBuf<'a> {
    #[inline]
    /// Initiate graceful io stream shutdown
    pub fn want_shutdown(&self) {
        self.io.want_shutdown()
    }

    #[inline]
    /// Get reference to source write buffer
    pub fn get_src(&mut self) -> &mut BytesVec {
        if self.curr.1.is_none() {
            self.curr.1 = Some(self.io.memory_pool().get_write_buf());
        }
        self.curr.1.as_mut().unwrap()
    }

    #[inline]
    /// Take source write buffer
    pub fn take_src(&mut self) -> Option<BytesVec> {
        self.curr
            .1
            .take()
            .and_then(|b| if b.is_empty() { None } else { Some(b) })
    }

    #[inline]
    /// Set source write buffer
    pub fn set_src(&mut self, src: Option<BytesVec>) {
        if let Some(buf) = self.curr.1.take() {
            self.io.memory_pool().release_read_buf(buf);
        }
        if let Some(buf) = src {
            if buf.is_empty() {
                self.io.memory_pool().release_read_buf(buf);
            } else {
                self.curr.1 = Some(buf);
            }
        }
    }

    #[inline]
    /// Get reference to destination write buffer
    pub fn with_dst_buf<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        if self.next.1.is_none() {
            self.next.1 = Some(self.io.memory_pool().get_write_buf());
        }
        let buf = self.next.1.as_mut().unwrap();
        let r = f(buf);
        self.need_write |= !buf.is_empty();
        r
    }

    #[inline]
    /// Take destination write buffer
    pub fn take_dst(&mut self) -> BytesVec {
        self.next
            .1
            .take()
            .unwrap_or_else(|| self.io.memory_pool().get_write_buf())
    }

    #[inline]
    /// Set destination write buffer
    pub fn set_dst(&mut self, dst: Option<BytesVec>) {
        if let Some(buf) = self.next.1.take() {
            self.io.memory_pool().release_write_buf(buf);
        }
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_write_buf(dst);
            } else {
                self.need_write |= !dst.is_empty();
                self.next.1 = Some(dst);
            }
        }
    }

    #[inline]
    pub fn with_read_buf<'b, F, R>(&'b mut self, f: F) -> R
    where
        F: FnOnce(&mut ReadBuf<'b>) -> R,
    {
        let mut buf = ReadBuf {
            io: self.io,
            curr: self.curr,
            next: self.next,
            nbytes: 0,
            need_write: self.need_write,
        };
        let result = f(&mut buf);
        self.need_write = buf.need_write;
        result
    }
}
