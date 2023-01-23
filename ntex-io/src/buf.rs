use ntex_bytes::{BytesVec, PoolRef};
use smallvec::SmallVec;

use crate::IoRef;

#[derive(Debug)]
pub struct Stack {
    pub(crate) buffers: SmallVec<[(Option<BytesVec>, Option<BytesVec>); 4]>,
}

impl Stack {
    pub(crate) fn new() -> Self {
        let mut buffers = SmallVec::with_capacity(4);
        buffers.push((None, None));

        Self { buffers }
    }

    pub(crate) fn add_layer(&mut self) {
        self.buffers.insert(0, (None, None));
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
        let pos = idx + 1;
        if self.buffers.len() > pos {
            let (curr, next) = self.buffers.split_at_mut(pos);
            let mut buf = ReadBuf {
                io,
                nbytes,
                curr: &mut curr[idx],
                next: &mut next[0],
            };
            f(&mut buf)
        } else {
            let mut val1 = (self.buffers[idx].0.take(), None);
            let mut val2 = (None, self.buffers[idx].1.take());

            let mut buf = ReadBuf {
                io,
                nbytes,
                curr: &mut val1,
                next: &mut val2,
            };
            let result = f(&mut buf);

            self.buffers[idx].0 = val1.0;
            self.buffers[idx].1 = val2.1;
            result
        }
    }

    pub(crate) fn write_buf<F, R>(&mut self, io: &IoRef, idx: usize, f: F) -> R
    where
        F: FnOnce(&mut WriteBuf<'_>) -> R,
    {
        let pos = idx + 1;
        if self.buffers.len() > pos {
            let (curr, next) = self.buffers.split_at_mut(pos);
            let mut buf = WriteBuf {
                io,
                curr: &mut curr[idx],
                next: &mut next[0],
            };
            f(&mut buf)
        } else {
            let mut val1 = (self.buffers[idx].0.take(), None);
            let mut val2 = (None, self.buffers[idx].1.take());

            let mut buf = WriteBuf {
                io,
                curr: &mut val1,
                next: &mut val2,
            };
            let result = f(&mut buf);

            self.buffers[idx].0 = val1.0;
            self.buffers[idx].1 = val2.1;
            result
        }
    }

    pub(crate) fn first_read_buf_size(&self) -> usize {
        self.buffers[0].0.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    pub(crate) fn first_read_buf(&mut self) -> &mut Option<BytesVec> {
        &mut self.buffers[0].0
    }

    pub(crate) fn first_write_buf(&mut self, io: &IoRef) -> &mut BytesVec {
        if self.buffers[0].1.is_none() {
            self.buffers[0].1 = Some(io.memory_pool().get_write_buf());
        }
        self.buffers[0].1.as_mut().unwrap()
    }

    pub(crate) fn last_read_buf(&mut self) -> &mut Option<BytesVec> {
        let idx = self.buffers.len() - 1;
        &mut self.buffers[idx].0
    }

    pub(crate) fn last_write_buf(&mut self) -> &mut Option<BytesVec> {
        let idx = self.buffers.len() - 1;
        &mut self.buffers[idx].1
    }

    pub(crate) fn last_write_buf_size(&self) -> usize {
        let idx = self.buffers.len() - 1;
        self.buffers[idx].1.as_ref().map(|b| b.len()).unwrap_or(0)
    }

    pub(crate) fn set_last_write_buf(&mut self, buf: BytesVec) {
        let idx = self.buffers.len() - 1;
        self.buffers[idx].1 = Some(buf);
    }

    pub(crate) fn release(&mut self, pool: PoolRef) {
        for buf in &mut self.buffers {
            if let Some(buf) = buf.0.take() {
                pool.release_read_buf(buf);
            }
            if let Some(buf) = buf.1.take() {
                pool.release_write_buf(buf);
            }
        }
    }

    pub(crate) fn set_memory_pool(&mut self, pool: PoolRef) {
        for buf in &mut self.buffers {
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
    pub(crate) curr: &'a mut (Option<BytesVec>, Option<BytesVec>),
    pub(crate) next: &'a mut (Option<BytesVec>, Option<BytesVec>),
    pub(crate) nbytes: usize,
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
        if let Some(src) = src {
            if src.is_empty() {
                self.io.memory_pool().release_read_buf(src);
            } else {
                if let Some(b) = self.next.0.take() {
                    self.io.memory_pool().release_read_buf(b);
                }
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
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_read_buf(dst);
            } else {
                if let Some(b) = self.curr.0.take() {
                    self.io.memory_pool().release_read_buf(b);
                }
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
        };
        f(&mut buf)
    }
}

#[derive(Debug)]
pub struct WriteBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a mut (Option<BytesVec>, Option<BytesVec>),
    pub(crate) next: &'a mut (Option<BytesVec>, Option<BytesVec>),
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
        if let Some(b) = self.curr.1.take() {
            self.io.memory_pool().release_read_buf(b);
        }
        self.curr.1 = src;
    }

    #[inline]
    /// Get reference to destination write buffer
    pub fn get_dst(&mut self) -> &mut BytesVec {
        if self.next.1.is_none() {
            self.next.1 = Some(self.io.memory_pool().get_write_buf());
        }
        self.next.1.as_mut().unwrap()
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
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_write_buf(dst);
            } else {
                if let Some(b) = self.next.1.take() {
                    self.io.memory_pool().release_write_buf(b);
                }
                self.next.1 = Some(dst);
            }
        }
    }

    #[inline]
    /// Get reference to source and destination buffers (src, dst)
    pub fn get_pair(&mut self) -> (&mut BytesVec, &mut BytesVec) {
        if self.curr.1.is_none() {
            self.curr.1 = Some(self.io.memory_pool().get_write_buf());
        }
        if self.next.1.is_none() {
            self.next.1 = Some(self.io.memory_pool().get_write_buf());
        }
        (self.curr.1.as_mut().unwrap(), self.next.1.as_mut().unwrap())
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
        };
        f(&mut buf)
    }
}
