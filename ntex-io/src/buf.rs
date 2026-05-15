use std::{cell::Cell, fmt, io, task::Poll};

use ntex_bytes::{BytePageSize, BytePages, BytesMut};

use crate::{IoConfig, IoRef};

pub(crate) struct Stack {
    buffers: Cell<Option<Box<[StackBuffer]>>>,
}

#[derive(Debug, Default)]
pub(crate) struct StackBuffer {
    read: Option<BytesMut>,
    write: BytePages,
}

impl fmt::Debug for Stack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_buffers(|buffers| {
            f.debug_struct("Stack")
                .field("len", &buffers.len())
                .field("buffers", &buffers)
                .finish()
        })
    }
}

impl Stack {
    pub(crate) fn new(size: BytePageSize) -> Self {
        Self {
            buffers: Cell::new(Some(
                vec![
                    StackBuffer {
                        read: None,
                        write: BytePages::new(size),
                    },
                    StackBuffer {
                        read: None,
                        write: BytePages::new(size),
                    },
                ]
                .into_boxed_slice(),
            )),
        }
    }

    pub(crate) fn set_page_size(&self, size: BytePageSize) {
        self.with_buffers(|buffers| {
            for b in &mut buffers.iter_mut() {
                b.write.set_page_size(size);
            }
        });
    }

    pub(crate) fn add_layer(&self) {
        let mut buffers = self.buffers.take().unwrap().into_vec();
        let buf = StackBuffer {
            read: None,
            write: BytePages::new(buffers[0].write.page_size()),
        };
        buffers.insert(0, buf);
        self.buffers.set(Some(buffers.into_boxed_slice()));
    }

    fn with_buffers<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut [StackBuffer]) -> R,
    {
        if let Some(mut buffers) = self.buffers.take() {
            let result = f(&mut buffers);
            self.buffers.set(Some(buffers));
            result
        } else {
            panic!("Nested call to .with_buffers()");
        }
    }

    fn with_first_level<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StackBuffer) -> R,
    {
        self.with_buffers(|buffers| f(&mut buffers[0]))
    }

    fn with_last_level<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StackBuffer) -> R,
    {
        self.with_buffers(|buffers| {
            let idx = buffers.len() - 2;
            f(&mut buffers[idx])
        })
    }

    pub(crate) fn with_read_dst<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.with_first_level(|buf| {
            let mut rb = buf.read.take().unwrap_or_else(|| io.cfg().read_buf().get());

            let result = f(&mut rb);

            // check nested updates
            if buf.read.take().is_some() {
                log::error!("Nested read io operation is detected");
                io.terminate();
            }

            if rb.is_empty() {
                io.cfg().read_buf().release(rb);
            } else {
                buf.read = Some(rb);
            }
            result
        })
    }

    pub(crate) fn write_buf_size(&self) -> usize {
        self.with_buffers(|buffers| {
            // check size for first level because delayed filter processing
            if buffers.len() == 2 {
                buffers[0].write.len()
            } else {
                buffers[0].write.len() + buffers[buffers.len() - 2].write.len()
            }
        })
    }

    pub(crate) fn with_write_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.with_first_level(|buf| f(&mut buf.write))
    }

    pub(crate) fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.with_last_level(|buf| f(&mut buf.write))
    }

    pub(crate) fn read_dst_size(&self) -> usize {
        self.with_first_level(|buf| buf.read.as_ref().map_or(0, BytesMut::len))
    }

    pub(crate) fn with_filter<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut FilterCtx<'_>) -> R,
    {
        self.with_buffers(|buffers| {
            let mut ctx = FilterCtx {
                io,
                buffers,
                idx: 0,
                nbytes: 0,
                st: FilterUpdates {
                    wants_write: false,
                    notify: false,
                },
            };
            f(&mut ctx)
        })
    }

    pub(crate) fn get_read_buf(&self) -> Option<BytesMut> {
        self.with_last_level(|buffer| buffer.read.take())
    }

    pub(crate) fn set_read_buf(&self, buf: BytesMut, cfg: &IoConfig) {
        self.with_last_level(move |buffer| {
            if let Some(mut first_buf) = buffer.read.take() {
                first_buf.extend_from_slice(&buf);
                cfg.read_buf().release(buf);
                buffer.read = Some(first_buf);
            } else if !buf.is_empty() {
                buffer.read = Some(buf);
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
        self.with_buffers(move |buffers| {
            let mut ctx = FilterCtx {
                io,
                buffers,
                nbytes,
                idx: 0,
                st: FilterUpdates {
                    wants_write: false,
                    notify: false,
                },
            };
            let result = io.filter().process_read_buf(&mut ctx);
            result.map(|()| ctx.st)
        })
    }

    pub(crate) fn process_write_buf(&self, io: &IoRef) -> io::Result<()> {
        self.with_buffers(move |buffers| {
            if buffers[0].write.is_empty() {
                Ok(())
            } else {
                let mut ctx = FilterCtx {
                    io,
                    buffers,
                    idx: 0,
                    nbytes: 0,
                    st: FilterUpdates {
                        wants_write: true,
                        notify: false,
                    },
                };
                io.filter().process_write_buf(&mut ctx)
            }
        })
    }

    pub(crate) fn process_write_buf_force(&self, io: &IoRef) -> io::Result<()> {
        self.with_buffers(move |buffers| {
            let mut ctx = FilterCtx {
                io,
                buffers,
                idx: 0,
                nbytes: 0,
                st: FilterUpdates {
                    wants_write: true,
                    notify: false,
                },
            };
            io.filter().process_write_buf(&mut ctx)
        })
    }

    pub(crate) fn process_shutdown(&self, io: &IoRef) -> io::Result<Poll<()>> {
        self.process_write_buf(io)?;
        self.with_filter(io, |ctx| io.filter().shutdown(ctx))
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct FilterUpdates {
    pub(crate) wants_write: bool,
    pub(crate) notify: bool,
}

#[derive(Debug)]
pub struct FilterCtx<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) idx: usize,
    pub(crate) nbytes: usize,
    pub(crate) buffers: &'a mut [StackBuffer],
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
        let (left, right) = self.buffers.split_at_mut(self.idx + 1);
        let mut buf = FilterBuf {
            io: self.io,
            curr: &mut left[self.idx],
            next: &mut right[0],
            wants_write: self.st.wants_write,
        };
        let result = f(&mut buf);
        if buf.wants_write {
            self.st.wants_write = true;
        }
        result
    }

    #[inline]
    /// Returns the size of the last read buffer in the chain.
    pub fn read_dst_size(&self) -> usize {
        self.buffers[0].read.as_ref().map_or(0, BytesMut::len)
    }

    #[inline]
    /// Returns the size of the last write buffer in the chain.
    pub fn write_dst_size(&mut self) -> usize {
        self.buffers[self.buffers.len() - 2].write.len()
    }

    pub(crate) fn clear_write_buf(&mut self) {
        self.buffers[self.idx].write.clear();
    }
}

#[derive(Debug)]
pub struct FilterBuf<'a> {
    pub(crate) io: &'a IoRef,
    pub(crate) curr: &'a mut StackBuffer,
    pub(crate) next: &'a mut StackBuffer,
    pub(crate) wants_write: bool,
}

impl FilterBuf<'_> {
    #[inline]
    /// Gets the I/O tag.
    pub fn tag(&self) -> &'static str {
        self.io.tag()
    }

    #[inline]
    /// Returns a reference to the source read buffer.
    pub fn read_src(&mut self) -> &mut Option<BytesMut> {
        &mut self.next.read
    }

    #[inline]
    /// Returns a reference to the destination read buffer.
    pub fn read_dst(&mut self) -> &mut Option<BytesMut> {
        &mut self.curr.read
    }

    /// Returns references to the source and destination buffers.
    pub fn with_buffers<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(
            &IoRef,
            &mut Option<BytesMut>,
            &mut Option<BytesMut>,
            &mut BytePages,
            &mut BytePages,
        ) -> R,
    {
        let mut read_src = self.next.read.take();
        let mut read_dst = self.curr.read.take();
        let write_len = if self.wants_write { 0 } else { self.next.write.len() };

        let result = f(
            self.io,
            &mut read_src,
            &mut read_dst,
            &mut self.curr.write,
            &mut self.next.write,
        );

        if !self.wants_write && self.next.write.len() > write_len {
            self.wants_write = true;
        }

        if let Some(b) = read_src {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.next.read = Some(b);
            }
        }
        if let Some(b) = read_dst {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.curr.read = Some(b);
            }
        }

        result
    }

    /// Returns references to the source and destination read buffers.
    pub fn with_read_buffers<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&IoRef, &mut Option<BytesMut>, &mut BytesMut) -> R,
    {
        let mut read_src = self.next.read.take();
        let mut read_dst = self
            .curr
            .read
            .take()
            .unwrap_or_else(|| self.io.cfg().read_buf().get());

        let result = f(self.io, &mut read_src, &mut read_dst);

        if let Some(b) = read_src {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                self.next.read = Some(b);
            }
        }
        if read_dst.is_empty() {
            self.io.cfg().read_buf().release(read_dst);
        } else {
            self.curr.read = Some(read_dst);
        }

        result
    }

    #[inline]
    /// Returns references to the source and destination write buffers.
    pub fn with_write_buffers<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&IoRef, &mut BytePages, &mut BytePages) -> R,
    {
        let write_len = if self.wants_write { 0 } else { self.next.write.len() };
        let result = f(self.io, &mut self.curr.write, &mut self.next.write);

        if !self.wants_write && self.next.write.len() > write_len {
            self.wants_write = true;
        }
        result
    }
}
