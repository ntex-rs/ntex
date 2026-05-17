use std::{cell::Cell, fmt, io, task::Poll};

use ntex_bytes::{BytePageSize, BytePages, BytesMut};

use crate::{IoConfig, IoRef};

pub(crate) struct Stack {
    buffers: Vec<Buffer>,
}

#[derive(Default)]
struct Buffer {
    read: Cell<Option<BytesMut>>,
    write: Cell<Option<BytePages>>,
}

impl fmt::Debug for Stack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stack")
            .field("len", &self.buffers.len())
            .finish()
    }
}

impl Stack {
    pub(crate) fn new(size: BytePageSize) -> Self {
        Self {
            buffers: vec![
                Buffer {
                    read: Cell::new(None),
                    write: Cell::new(Some(BytePages::new(size))),
                },
                Buffer {
                    read: Cell::new(None),
                    write: Cell::new(Some(BytePages::new(size))),
                },
            ],
        }
    }

    pub(crate) fn set_page_size(&self, size: BytePageSize) {
        for b in &self.buffers {
            b.with_write(|b| b.set_page_size(size));
        }
    }

    pub(crate) fn add_layer(&mut self, page_size: BytePageSize) {
        self.buffers.insert(
            0,
            Buffer {
                read: Cell::new(None),
                write: Cell::new(Some(BytePages::new(page_size))),
            },
        );
    }

    fn with_first<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Buffer) -> R,
    {
        f(&self.buffers[0])
    }

    fn with_last<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Buffer) -> R,
    {
        f(&self.buffers[self.buffers.len() - 2])
    }

    pub(crate) fn with_read_dst<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.with_first(|buf| buf.with_read(io, f))
    }

    pub(crate) fn write_buf_size(&self) -> usize {
        // check size for first level because delayed filter processing
        if self.buffers.len() == 2 {
            self.buffers[0].write_len()
        } else {
            self.buffers[0].write_len() + self.buffers[self.buffers.len() - 2].write_len()
        }
    }

    pub(crate) fn with_write_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.buffers[0].with_write(f)
    }

    pub(crate) fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.buffers[self.buffers.len() - 2].with_write(f)
    }

    pub(crate) fn read_dst_size(&self) -> usize {
        self.buffers[0].read_len()
    }

    pub(crate) fn with_filter<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut FilterCtx<'_>) -> R,
    {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            nbytes: 0,
            stack: self,
            st: FilterUpdates {
                wants_write: false,
                notify: false,
            },
        };
        f(&mut ctx)
    }

    pub(crate) fn get_read_buf(&self) -> Option<BytesMut> {
        self.with_last(|buffer| buffer.read.take())
    }

    pub(crate) fn set_read_buf(&self, buf: BytesMut, cfg: &IoConfig) {
        self.with_last(move |buffer| {
            if let Some(mut first_buf) = buffer.read.take() {
                first_buf.extend_from_slice(&buf);
                cfg.read_buf().release(buf);
                buffer.read.set(Some(first_buf));
            } else if !buf.is_empty() {
                buffer.read.set(Some(buf));
            } else {
                cfg.read_buf().release(buf);
            }
        });
    }

    pub(crate) fn process_read_buf(
        &self,
        io: &IoRef,
        nbytes: usize,
    ) -> io::Result<FilterUpdates> {
        let mut ctx = FilterCtx {
            io,
            nbytes,
            idx: 0,
            stack: self,
            st: FilterUpdates {
                wants_write: false,
                notify: false,
            },
        };
        let result = io.filter().process_read_buf(&mut ctx);
        result.map(|()| ctx.st)
    }

    pub(crate) fn process_write_buf(&self, io: &IoRef) -> io::Result<()> {
        if self.buffers[0].is_write_empty() {
            Ok(())
        } else {
            let mut ctx = FilterCtx {
                io,
                idx: 0,
                nbytes: 0,
                stack: self,
                st: FilterUpdates {
                    wants_write: true,
                    notify: false,
                },
            };
            io.filter().process_write_buf(&mut ctx)
        }
    }

    pub(crate) fn process_write_buf_force(&self, io: &IoRef) -> io::Result<()> {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            nbytes: 0,
            stack: self,
            st: FilterUpdates {
                wants_write: true,
                notify: false,
            },
        };
        io.filter().process_write_buf(&mut ctx)
    }

    pub(crate) fn process_shutdown(&self, io: &IoRef) -> io::Result<Poll<()>> {
        self.process_write_buf(io)?;
        self.with_filter(io, |ctx| io.filter().shutdown(ctx))
    }
}

impl Buffer {
    fn is_write_empty(&self) -> bool {
        self.with_write(|b| b.is_empty())
    }

    fn read_len(&self) -> usize {
        if let Some(rb) = self.read.take() {
            let l = rb.len();
            self.read.set(Some(rb));
            l
        } else {
            0
        }
    }

    fn write_len(&self) -> usize {
        self.with_write(|b| b.len())
    }

    fn with_read<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut rb = self
            .read
            .take()
            .unwrap_or_else(|| io.cfg().read_buf().get());
        let result = f(&mut rb);

        // check nested updates
        if self.read.take().is_some() {
            log::error!("Nested read io operation is detected");
            io.terminate();
        }

        if rb.is_empty() {
            io.cfg().read_buf().release(rb);
        } else {
            self.read.set(Some(rb));
        }
        result
    }

    fn with_write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        let mut wb = self.write.take().unwrap();
        let result = f(&mut wb);
        self.write.set(Some(wb));
        result
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct FilterUpdates {
    pub(crate) wants_write: bool,
    pub(crate) notify: bool,
}

#[derive(Debug)]
pub struct FilterCtx<'a> {
    io: &'a IoRef,
    idx: usize,
    nbytes: usize,
    stack: &'a Stack,
    st: FilterUpdates,
}

impl FilterCtx<'_> {
    #[inline]
    /// Gets a reference to the I/O object.
    pub fn io(&self) -> &IoRef {
        self.io
    }

    #[inline]
    /// Gets the I/O tag.
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

    #[inline]
    /// Gets new bytes count for read buffer.
    pub fn new_read_bytes(&self) -> usize {
        self.nbytes
    }

    #[inline]
    /// Notifies about readiness changes.
    pub fn notify(&mut self) {
        self.st.notify = true;
    }

    #[inline]
    /// Returns the filter context for the next filter in the chain.
    pub fn with_next<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.idx += 1;
        let res = f(self);
        self.idx -= 1;
        res
    }

    #[inline]
    /// Returns the filter buffer.
    pub fn with_buffer<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        let mut buf = FilterBuf {
            io: self.io,
            curr: &self.stack.buffers[self.idx],
            next: &self.stack.buffers[self.idx + 1],
            wants_write: Cell::new(self.st.wants_write),
        };
        let result = f(&mut buf);
        if buf.wants_write.get() {
            self.st.wants_write = true;
        }
        result
    }

    #[inline]
    /// Returns the size of the last read buffer in the chain.
    pub fn read_dst_size(&self) -> usize {
        self.stack.buffers[0].read_len()
    }

    #[inline]
    /// Returns the size of the last write buffer in the chain.
    pub fn write_dst_size(&mut self) -> usize {
        self.stack.buffers[self.stack.buffers.len() - 2].write_len()
    }

    pub(crate) fn clear_write_buf(&mut self) {
        self.stack.buffers[self.idx].with_write(BytePages::clear);
    }
}

#[derive(Debug)]
pub struct FilterBuf<'a> {
    io: &'a IoRef,
    curr: &'a Buffer,
    next: &'a Buffer,
    wants_write: Cell<bool>,
}

impl FilterBuf<'_> {
    #[inline]
    /// Gets a reference to the I/O object.
    pub fn io(&self) -> &IoRef {
        self.io
    }

    #[inline]
    /// Gets the I/O tag.
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

    /// Returns references to the source read buffer.
    pub fn with_read_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesMut>) -> R,
    {
        let mut read_src = self.next.read.take();
        let result = f(&mut read_src);

        if let Some(b) = read_src {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.next.read.set(Some(b));
            }
        }
        result
    }

    /// Returns references to the source and destination read buffers.
    pub fn with_read_buffers<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesMut>, &mut BytesMut) -> R,
    {
        let mut read_src = self.next.read.take();
        let mut read_dst = self
            .curr
            .read
            .take()
            .unwrap_or_else(|| self.io.cfg().read_buf().get());

        let result = f(&mut read_src, &mut read_dst);

        if let Some(b) = read_src {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.next.read.set(Some(b));
            }
        }
        if read_dst.is_empty() {
            self.io.cfg().read_buf().release(read_dst);
        } else {
            self.curr.read.set(Some(read_dst));
        }

        result
    }

    #[inline]
    /// Returns references to the source and destination write buffers.
    pub fn with_write_buffers<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages, &mut BytePages) -> R,
    {
        let mut write_curr = self.curr.write.take().unwrap();
        let mut write_next = self.next.write.take().unwrap();
        let write_len = if self.wants_write.get() {
            0
        } else {
            write_next.len()
        };

        let result = f(&mut write_curr, &mut write_next);

        if !self.wants_write.get() && write_next.len() > write_len {
            self.wants_write.set(true);
        }

        self.curr.write.set(Some(write_curr));
        self.next.write.set(Some(write_next));
        result
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let read = self.read.take();
        let write = self.write.take();

        let result = f
            .debug_struct("Buffer")
            .field("read", &read)
            .field("write", &write)
            .finish();
        self.read.set(read);
        self.write.set(write);
        result
    }
}
