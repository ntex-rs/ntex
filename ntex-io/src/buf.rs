use std::{cell::Cell, fmt};

use ntex_bytes::BytesVec;
use ntex_util::future::Either;

use crate::{IoRef, cfg::BufConfig};

#[derive(Default)]
pub(crate) struct Buffer {
    read: Cell<Option<BytesVec>>,
    write: Cell<Option<BytesVec>>,
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b0 = self.read.take();
        let b1 = self.write.take();
        let res = f
            .debug_struct("Buffer")
            .field("read", &b0)
            .field("write", &b1)
            .finish();
        self.read.set(b0);
        self.write.set(b1);
        res
    }
}

const INLINE_SIZE: usize = 3;

#[derive(Debug)]
pub(crate) struct Stack {
    len: usize,
    buffers: Either<[Buffer; INLINE_SIZE], Vec<Buffer>>,
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
                if self.len == INLINE_SIZE {
                    let mut vec = vec![Buffer {
                        read: Cell::new(None),
                        write: Cell::new(None),
                    }];
                    for item in b.iter_mut().take(self.len) {
                        vec.push(Buffer {
                            read: Cell::new(item.read.take()),
                            write: Cell::new(item.write.take()),
                        });
                    }
                    self.len += 1;
                    self.buffers = Either::Right(vec);
                } else {
                    let mut idx = self.len;
                    while idx > 0 {
                        let item = Buffer {
                            read: Cell::new(b[idx - 1].read.take()),
                            write: Cell::new(b[idx - 1].write.take()),
                        };
                        b[idx] = item;
                        idx -= 1;
                    }
                    b[0] = Buffer {
                        read: Cell::new(None),
                        write: Cell::new(None),
                    };
                    self.len += 1;
                }
            }
            Either::Right(vec) => {
                self.len += 1;
                vec.insert(
                    0,
                    Buffer {
                        read: Cell::new(None),
                        write: Cell::new(None),
                    },
                );
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
            f(&buffers[idx], &Buffer::default())
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

    pub(crate) fn get_read_source(&self) -> Option<BytesVec> {
        self.get_last_level().read.take()
    }

    pub(crate) fn set_read_source(&self, io: &IoRef, buf: BytesVec) {
        if buf.is_empty() {
            io.cfg().read_buf().release(buf);
        } else {
            self.get_last_level().read.set(Some(buf));
        }
    }

    pub(crate) fn with_read_destination<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let item = self.get_first_level();
        let mut rb = item
            .read
            .take()
            .unwrap_or_else(|| io.cfg().read_buf().get());

        let result = f(&mut rb);

        // check nested updates
        if item.read.take().is_some() {
            log::error!("Nested read io operation is detected");
            io.force_close();
        }

        if rb.is_empty() {
            io.cfg().read_buf().release(rb);
        } else {
            item.read.set(Some(rb));
        }
        result
    }

    pub(crate) fn get_write_destination(&self) -> Option<BytesVec> {
        self.get_last_level().write.take()
    }

    pub(crate) fn set_write_destination(&self, buf: BytesVec) -> Option<BytesVec> {
        let b = self.get_last_level().write.take();
        if b.is_some() {
            self.get_last_level().write.set(b);
            Some(buf)
        } else {
            self.get_last_level().write.set(Some(buf));
            None
        }
    }

    pub(crate) fn with_write_source<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesVec) -> R,
    {
        let item = self.get_first_level();
        let mut wb = item
            .write
            .take()
            .unwrap_or_else(|| io.cfg().write_buf().get());

        let result = f(&mut wb);
        if wb.is_empty() {
            io.cfg().write_buf().release(wb);
        } else {
            item.write.set(Some(wb));
        }
        result
    }

    pub(crate) fn with_write_destination<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(Option<&mut BytesVec>) -> R,
    {
        let item = self.get_last_level();
        let mut wb = item.write.take();

        let result = f(wb.as_mut());

        // check nested updates
        if item.write.take().is_some() {
            log::error!("Nested write io operation is detected");
            io.force_close();
        }

        if let Some(b) = wb {
            if b.is_empty() {
                io.cfg().write_buf().release(b);
            } else {
                item.write.set(Some(b));
            }
        }
        result
    }

    pub(crate) fn read_destination_size(&self) -> usize {
        let item = self.get_first_level();
        let rb = item.read.take();
        let size = rb.as_ref().map(|b| b.len()).unwrap_or(0);
        item.read.set(rb);
        size
    }

    pub(crate) fn write_destination_size(&self) -> usize {
        let item = self.get_last_level();
        let wb = item.write.take();
        let size = wb.as_ref().map(|b| b.len()).unwrap_or(0);
        item.write.set(wb);
        size
    }
}

#[derive(Copy, Clone, Debug)]
pub struct FilterCtx<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) stack: &'a Stack,
    pub(crate) idx: usize,
}

impl<'a> FilterCtx<'a> {
    pub(crate) fn new(io: &'a IoRef, stack: &'a Stack) -> Self {
        Self { io, stack, idx: 0 }
    }

    #[inline]
    /// Get io
    pub fn io(&self) -> &IoRef {
        self.io
    }

    #[inline]
    /// Get io tag
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

    #[inline]
    /// Get filter ctx for next filter in chain
    pub fn next(&self) -> Self {
        Self {
            io: self.io,
            stack: self.stack,
            idx: self.idx + 1,
        }
    }

    #[inline]
    /// Get current read buffer
    pub fn read_buf<F, R>(&self, nbytes: usize, f: F) -> R
    where
        F: FnOnce(&ReadBuf<'_>) -> R,
    {
        self.stack.get_buffers(self.idx, |curr, next| {
            let buf = ReadBuf {
                nbytes,
                curr,
                next,
                io: self.io,
                need_write: Cell::new(false),
            };
            f(&buf)
        })
    }

    #[inline]
    /// Get current write buffer
    pub fn write_buf<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&WriteBuf<'_>) -> R,
    {
        self.stack.get_buffers(self.idx, |curr, next| {
            let buf = WriteBuf {
                curr,
                next,
                io: self.io,
                need_write: Cell::new(false),
            };
            f(&buf)
        })
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
    /// Get buffer params
    pub fn cfg(&self) -> &BufConfig {
        self.io.cfg().read_buf()
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
        self.io.cfg().read_buf().resize(buf);
    }

    #[inline]
    /// Get reference to source read buffer
    pub fn with_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesVec>) -> R,
    {
        let mut buf = self.next.read.take();
        let result = f(&mut buf);

        if let Some(b) = buf {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.next.read.set(Some(b));
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
        let mut rb = self
            .curr
            .read
            .take()
            .unwrap_or_else(|| self.io.cfg().read_buf().get());

        let result = f(&mut rb);
        if rb.is_empty() {
            self.io.cfg().read_buf().release(rb);
        } else {
            self.curr.read.set(Some(rb));
        }
        result
    }

    #[inline]
    /// Take source read buffer
    pub fn take_src(&self) -> Option<BytesVec> {
        self.next.read.take().and_then(|b| {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
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
                self.io.cfg().read_buf().release(src);
            } else if let Some(mut buf) = self.next.read.take() {
                buf.extend_from_slice(&src);
                self.next.read.set(Some(buf));
                self.io.cfg().read_buf().release(src);
            } else {
                self.next.read.set(Some(src));
            }
        }
    }

    #[inline]
    /// Take destination read buffer
    pub fn take_dst(&self) -> BytesVec {
        self.curr
            .read
            .take()
            .unwrap_or_else(|| self.io.cfg().read_buf().get())
    }

    #[inline]
    /// Set destination read buffer
    pub fn set_dst(&self, dst: Option<BytesVec>) {
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.cfg().read_buf().release(dst);
            } else if let Some(mut buf) = self.curr.read.take() {
                buf.extend_from_slice(&dst);
                self.curr.read.set(Some(buf));
                self.io.cfg().read_buf().release(dst);
            } else {
                self.curr.read.set(Some(dst));
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
    /// Get buffer params
    pub fn cfg(&self) -> &BufConfig {
        self.io.cfg().write_buf()
    }

    #[inline]
    /// Initiate graceful io stream shutdown
    pub fn want_shutdown(&self) {
        self.io.want_shutdown()
    }

    #[inline]
    /// Make sure buffer has enough free space
    pub fn resize_buf(&self, buf: &mut BytesVec) {
        self.io.cfg().write_buf().resize(buf);
    }

    #[inline]
    /// Get reference to source write buffer
    pub fn with_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesVec>) -> R,
    {
        let mut wb = self.curr.write.take();
        let result = f(&mut wb);
        if let Some(b) = wb {
            if b.is_empty() {
                self.io.cfg().write_buf().release(b);
            } else {
                self.curr.write.set(Some(b));
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
        let mut wb = self
            .next
            .write
            .take()
            .unwrap_or_else(|| self.io.cfg().write_buf().get());

        let total = wb.len();
        let result = f(&mut wb);

        if wb.is_empty() {
            self.io.cfg().write_buf().release(wb);
        } else {
            self.need_write
                .set(self.need_write.get() | (total != wb.len()));
            self.next.write.set(Some(wb));
        }
        result
    }

    #[inline]
    /// Take source write buffer
    pub fn take_src(&self) -> Option<BytesVec> {
        self.curr.write.take().and_then(|b| {
            if b.is_empty() {
                self.io.cfg().write_buf().release(b);
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
                self.io.cfg().write_buf().release(src);
            } else if let Some(mut buf) = self.curr.write.take() {
                buf.extend_from_slice(&src);
                self.curr.write.set(Some(buf));
                self.io.cfg().write_buf().release(src);
            } else {
                self.curr.write.set(Some(src));
            }
        }
    }

    #[inline]
    /// Take destination write buffer
    pub fn take_dst(&self) -> BytesVec {
        self.next
            .write
            .take()
            .unwrap_or_else(|| self.io.cfg().write_buf().get())
    }

    #[inline]
    /// Set destination write buffer
    pub fn set_dst(&self, dst: Option<BytesVec>) {
        if let Some(dst) = dst {
            if dst.is_empty() {
                self.io.cfg().write_buf().release(dst);
            } else {
                self.need_write.set(true);

                if let Some(mut buf) = self.next.write.take() {
                    buf.extend_from_slice(&dst);
                    self.next.write.set(Some(buf));
                    self.io.cfg().write_buf().release(dst);
                } else {
                    self.next.write.set(Some(dst));
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
