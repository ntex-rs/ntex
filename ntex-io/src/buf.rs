use std::{cell::Cell, fmt};

use ntex_bytes::{BytesVec, PoolRef};
use ntex_util::future::Either;

use crate::IoRef;

#[derive(Default)]
pub(crate) struct Buffer(Cell<Option<BytesVec>>, Cell<Option<BytesVec>>);

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b0 = self.0.take();
        let b1 = self.1.take();
        let res = f
            .debug_struct("Buffer")
            .field("0", &b0)
            .field("1", &b1)
            .finish();
        self.0.set(b0);
        self.1.set(b1);
        res
    }
}

#[derive(Debug)]
pub struct Stack {
    len: usize,
    buffers: Either<[Buffer; 3], Vec<Buffer>>,
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
                // move to vec
                if self.len == 3 {
                    let mut vec = vec![Buffer(Cell::new(None), Cell::new(None))];
                    for item in b.iter_mut().take(self.len) {
                        vec.push(Buffer(
                            Cell::new(item.0.take()),
                            Cell::new(item.1.take()),
                        ));
                    }
                    self.len += 1;
                    self.buffers = Either::Right(vec);
                } else {
                    let mut idx = self.len;
                    while idx > 0 {
                        let item = Buffer(
                            Cell::new(b[idx - 1].0.take()),
                            Cell::new(b[idx - 1].1.take()),
                        );
                        b[idx] = item;
                        idx -= 1;
                    }
                    b[0] = Buffer(Cell::new(None), Cell::new(None));
                    self.len += 1;
                }
            }
            Either::Right(vec) => {
                self.len += 1;
                vec.insert(0, Buffer(Cell::new(None), Cell::new(None)));
            }
        }
    }

    fn get_buffers<F, R>(&self, idx: usize, f: F) -> R
    where
        F: FnOnce(&Buffer, &Buffer) -> R,
    {
        let buffers = match self.buffers {
            Either::Left(ref b) => &b[..],
            Either::Right(ref b) => &b[..],
        };

        let next = idx + 1;
        if self.len > next {
            f(&buffers[idx], &buffers[next])
        } else {
            let curr = Buffer(Cell::new(buffers[idx].0.take()), Cell::new(None));
            let next = Buffer(Cell::new(None), Cell::new(buffers[idx].1.take()));

            let result = f(&curr, &next);
            buffers[idx].0.set(curr.0.take());
            buffers[idx].1.set(next.1.take());
            result
        }
    }

    fn get_first_level(&self) -> &Buffer {
        match &self.buffers {
            Either::Left(b) => &b[0],
            Either::Right(b) => &b[0],
        }
    }

    fn get_last_level(&self) -> &Buffer {
        match &self.buffers {
            Either::Left(b) => &b[self.len - 1],
            Either::Right(b) => &b[self.len - 1],
        }
    }

    pub(crate) fn read_buf<F, R>(&self, io: &IoRef, idx: usize, nbytes: usize, f: F) -> R
    where
        F: FnOnce(&ReadBuf<'_>) -> R,
    {
        self.get_buffers(idx, |curr, next| {
            let buf = ReadBuf {
                io,
                nbytes,
                curr,
                next,
                need_write: Cell::new(false),
            };
            f(&buf)
        })
    }

    pub(crate) fn write_buf<F, R>(&self, io: &IoRef, idx: usize, f: F) -> R
    where
        F: FnOnce(&WriteBuf<'_>) -> R,
    {
        self.get_buffers(idx, |curr, next| {
            let buf = WriteBuf {
                io,
                curr,
                next,
                need_write: Cell::new(false),
            };
            f(&buf)
        })
    }

    pub(crate) fn get_read_source(&self) -> Option<BytesVec> {
        self.get_last_level().0.take()
    }

    pub(crate) fn set_read_source(&self, io: &IoRef, buf: BytesVec) {
        if buf.is_empty() {
            io.memory_pool().release_read_buf(buf);
        } else {
            self.get_last_level().0.set(Some(buf));
        }
    }

    pub(crate) fn with_read_source<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let item = self.get_last_level();
        let mut rb = item.0.take();
        if rb.is_none() {
            rb = Some(io.memory_pool().get_read_buf());
        }

        let result = f(rb.as_mut().unwrap());
        if let Some(b) = rb {
            if b.is_empty() {
                io.memory_pool().release_read_buf(b);
            } else {
                item.0.set(Some(b));
            }
        }
        result
    }

    pub(crate) fn with_read_destination<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let item = self.get_first_level();
        let mut rb = item.0.take();
        if rb.is_none() {
            rb = Some(io.memory_pool().get_read_buf());
        }

        let result = f(rb.as_mut().unwrap());

        // check nested updates
        if item.0.take().is_some() {
            log::error!("Nested read io operation is detected");
            io.force_close();
        }

        if let Some(b) = rb {
            if b.is_empty() {
                io.memory_pool().release_read_buf(b);
            } else {
                item.0.set(Some(b));
            }
        }
        result
    }

    pub(crate) fn with_write_source<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let item = self.get_first_level();
        let mut wb = item.1.take();
        if wb.is_none() {
            wb = Some(io.memory_pool().get_write_buf());
        }

        let result = f(wb.as_mut().unwrap());
        if let Some(b) = wb {
            if b.is_empty() {
                io.memory_pool().release_write_buf(b);
            } else {
                item.1.set(Some(b));
            }
        }
        result
    }

    pub(crate) fn get_write_destination(&self) -> Option<BytesVec> {
        self.get_last_level().1.take()
    }

    pub(crate) fn set_write_destination(&self, buf: BytesVec) -> Option<BytesVec> {
        let b = self.get_last_level().1.take();
        if b.is_some() {
            self.get_last_level().1.set(b);
            Some(buf)
        } else {
            self.get_last_level().1.set(Some(buf));
            None
        }
    }

    pub(crate) fn with_write_destination<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(Option<&mut BytesVec>) -> R,
    {
        let item = self.get_last_level();
        let mut wb = item.1.take();

        let result = f(wb.as_mut());

        // check nested updates
        if item.1.take().is_some() {
            log::error!("Nested write io operation is detected");
            io.force_close();
        }

        if let Some(b) = wb {
            if b.is_empty() {
                io.memory_pool().release_write_buf(b);
            } else {
                item.1.set(Some(b));
            }
        }
        result
    }

    pub(crate) fn read_destination_size(&self) -> usize {
        let item = self.get_first_level();
        let rb = item.0.take();
        let size = rb.as_ref().map(|b| b.len()).unwrap_or(0);
        item.0.set(rb);
        size
    }

    pub(crate) fn write_destination_size(&self) -> usize {
        let item = self.get_last_level();
        let wb = item.1.take();
        let size = wb.as_ref().map(|b| b.len()).unwrap_or(0);
        item.1.set(wb);
        size
    }

    pub(crate) fn release(&self, pool: PoolRef) {
        let items = match &self.buffers {
            Either::Left(b) => &b[..],
            Either::Right(b) => &b[..],
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

    pub(crate) fn set_memory_pool(&self, pool: PoolRef) {
        let items = match &self.buffers {
            Either::Left(b) => &b[..],
            Either::Right(b) => &b[..],
        };
        for item in items {
            if let Some(mut b) = item.0.take() {
                pool.move_vec_in(&mut b);
                item.0.set(Some(b));
            }
            if let Some(mut b) = item.1.take() {
                pool.move_vec_in(&mut b);
                item.1.set(Some(b));
            }
        }
    }
}

#[derive(Debug)]
pub struct ReadBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a Buffer,
    pub(crate) next: &'a Buffer,
    pub(crate) nbytes: usize,
    pub(crate) need_write: Cell<bool>,
}

impl ReadBuf<'_> {
    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

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
    /// Make sure buffer has enough free space
    pub fn resize_buf(&self, buf: &mut BytesVec) {
        self.io.memory_pool().resize_read_buf(buf);
    }

    #[inline]
    /// Get reference to source read buffer
    pub fn with_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesVec>) -> R,
    {
        let mut item = self.next.0.take();
        let result = f(&mut item);

        if let Some(b) = item {
            if b.is_empty() {
                self.io.memory_pool().release_read_buf(b);
            } else {
                self.next.0.set(Some(b));
            }
        }
        result
    }

    #[inline]
    /// Get reference to destination read buffer
    pub fn with_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let mut item = self.curr.0.take();
        if item.is_none() {
            item = Some(self.io.memory_pool().get_read_buf());
        }
        let result = f(item.as_mut().unwrap());
        if let Some(b) = item {
            if b.is_empty() {
                self.io.memory_pool().release_read_buf(b);
            } else {
                self.curr.0.set(Some(b));
            }
        }
        result
    }

    #[inline]
    /// Take source read buffer
    pub fn take_src(&self) -> Option<BytesVec> {
        self.next.0.take().and_then(|b| {
            if b.is_empty() {
                self.io.memory_pool().release_read_buf(b);
                None
            } else {
                Some(b)
            }
        })
    }

    #[inline]
    /// Set source read buffer
    pub fn set_src(&self, src: Option<BytesVec>) {
        if let Some(src) = src {
            if src.is_empty() {
                self.io.memory_pool().release_read_buf(src);
            } else if let Some(mut buf) = self.next.0.take() {
                buf.extend_from_slice(&src);
                self.next.0.set(Some(buf));
                self.io.memory_pool().release_read_buf(src);
            } else {
                self.next.0.set(Some(src));
            }
        }
    }

    #[inline]
    /// Take destination read buffer
    pub fn take_dst(&self) -> BytesVec {
        self.curr
            .0
            .take()
            .unwrap_or_else(|| self.io.memory_pool().get_read_buf())
    }

    #[inline]
    /// Set destination read buffer
    pub fn set_dst(&self, dst: Option<BytesVec>) {
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_read_buf(dst);
            } else if let Some(mut buf) = self.curr.0.take() {
                buf.extend_from_slice(&dst);
                self.curr.0.set(Some(buf));
                self.io.memory_pool().release_read_buf(dst);
            } else {
                self.curr.0.set(Some(dst));
            }
        }
    }

    #[inline]
    pub fn with_write_buf<'b, F, R>(&'b self, f: F) -> R
    where
        F: FnOnce(&WriteBuf<'b>) -> R,
    {
        let mut buf = WriteBuf {
            io: self.io,
            curr: self.curr,
            next: self.next,
            need_write: Cell::new(self.need_write.get()),
        };
        let result = f(&mut buf);
        self.need_write.set(buf.need_write.get());
        result
    }
}

#[derive(Debug)]
pub struct WriteBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a Buffer,
    pub(crate) next: &'a Buffer,
    pub(crate) need_write: Cell<bool>,
}

impl WriteBuf<'_> {
    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

    #[inline]
    /// Initiate graceful io stream shutdown
    pub fn want_shutdown(&self) {
        self.io.want_shutdown()
    }

    #[inline]
    /// Make sure buffer has enough free space
    pub fn resize_buf(&self, buf: &mut BytesVec) {
        self.io.memory_pool().resize_write_buf(buf);
    }

    #[inline]
    /// Get reference to source write buffer
    pub fn with_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesVec>) -> R,
    {
        let mut item = self.curr.1.take();
        let result = f(&mut item);
        if let Some(b) = item {
            if b.is_empty() {
                self.io.memory_pool().release_write_buf(b);
            } else {
                self.curr.1.set(Some(b));
            }
        }
        result
    }

    #[inline]
    /// Get reference to destination write buffer
    pub fn with_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let mut item = self.next.1.take();
        if item.is_none() {
            item = Some(self.io.memory_pool().get_write_buf());
        }
        let buf = item.as_mut().unwrap();
        let total = buf.len();
        let result = f(buf);

        if buf.is_empty() {
            self.io.memory_pool().release_write_buf(item.unwrap());
        } else {
            self.need_write
                .set(self.need_write.get() | (total != buf.len()));
            self.next.1.set(item);
        }
        result
    }

    #[inline]
    /// Take source write buffer
    pub fn take_src(&self) -> Option<BytesVec> {
        self.curr.1.take().and_then(|b| {
            if b.is_empty() {
                self.io.memory_pool().release_write_buf(b);
                None
            } else {
                Some(b)
            }
        })
    }

    #[inline]
    /// Set source write buffer
    pub fn set_src(&self, src: Option<BytesVec>) {
        if let Some(src) = src {
            if src.is_empty() {
                self.io.memory_pool().release_write_buf(src);
            } else if let Some(mut buf) = self.curr.1.take() {
                buf.extend_from_slice(&src);
                self.curr.1.set(Some(buf));
                self.io.memory_pool().release_write_buf(src);
            } else {
                self.curr.1.set(Some(src));
            }
        }
    }

    #[inline]
    /// Take destination write buffer
    pub fn take_dst(&self) -> BytesVec {
        self.next
            .1
            .take()
            .unwrap_or_else(|| self.io.memory_pool().get_write_buf())
    }

    #[inline]
    /// Set destination write buffer
    pub fn set_dst(&self, dst: Option<BytesVec>) {
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.memory_pool().release_write_buf(dst);
            } else {
                self.need_write.set(true);

                if let Some(mut buf) = self.next.1.take() {
                    buf.extend_from_slice(&dst);
                    self.next.1.set(Some(buf));
                    self.io.memory_pool().release_write_buf(dst);
                } else {
                    self.next.1.set(Some(dst));
                }
            }
        }
    }

    #[inline]
    pub fn with_read_buf<'b, F, R>(&'b self, f: F) -> R
    where
        F: FnOnce(&ReadBuf<'b>) -> R,
    {
        let mut buf = ReadBuf {
            io: self.io,
            curr: self.curr,
            next: self.next,
            nbytes: 0,
            need_write: Cell::new(self.need_write.get()),
        };
        let result = f(&mut buf);
        self.need_write.set(buf.need_write.get());
        result
    }
}
