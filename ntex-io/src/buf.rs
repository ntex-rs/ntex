use std::{cell::Cell, cell::RefCell, fmt, io, iter, mem, task::Poll};

use ntex_bytes::{BytePageSize, BytePages, BytesMut};

use crate::{IoConfig, IoRef};

/// Buffers of the filter chain, ordered from the application toward the
/// transport.
///
/// Without a filter layer the application and the transport share `app`.
/// Each added layer becomes the outermost one: it gets a new `app` buffer, and
/// the previous `app` buffer, with any data buffered in it, moves one position
/// toward the transport. The first one becomes `wire`, further ones go to
/// `mid`. The layer at position `i` uses the buffers at positions `i` and
/// `i + 1`, the innermost layer writes to and reads from `wire`.
pub(crate) struct Stack {
    app: Buffer,
    mid: Vec<Buffer>,
    wire: Option<Buffer>,
}

struct Buffer {
    read: Cell<Option<BytesMut>>,
    write: RefCell<BytePages>,
}

impl fmt::Debug for Stack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stack")
            .field("layers", &self.layers())
            .finish()
    }
}

impl Stack {
    pub(crate) fn new(size: BytePageSize) -> Self {
        Self {
            app: Buffer::new(size),
            mid: Vec::new(),
            wire: None,
        }
    }

    /// Returns the number of installed filter layers.
    fn layers(&self) -> usize {
        self.wire.as_ref().map_or(0, |_| self.mid.len() + 1)
    }

    fn buffers(&self) -> impl Iterator<Item = &Buffer> {
        iter::once(&self.app)
            .chain(self.mid.iter())
            .chain(self.wire.iter())
    }

    /// Returns the buffer at position `idx`, counted from the application.
    fn get(&self, idx: usize) -> Option<&Buffer> {
        if idx == 0 {
            Some(&self.app)
        } else if let Some(buf) = self.mid.get(idx - 1) {
            Some(buf)
        } else if idx == self.mid.len() + 1 {
            self.wire.as_ref()
        } else {
            None
        }
    }

    /// Returns the transport-facing buffer.
    fn transport(&self) -> &Buffer {
        self.wire.as_ref().unwrap_or(&self.app)
    }

    pub(crate) fn set_page_size(&self, size: BytePageSize) {
        for b in self.buffers() {
            b.with_write_if_free(|b| b.set_page_size(size));
        }
    }

    pub(crate) fn add_layer(&mut self, page_size: BytePageSize) {
        let outer = mem::replace(&mut self.app, Buffer::new(page_size));
        if self.wire.is_none() {
            self.wire = Some(outer);
        } else {
            self.mid.insert(0, outer);
        }
    }

    pub(crate) fn with_read_src<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.transport().with_read(io, f)
    }

    pub(crate) fn with_read_dst<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.app.with_read(io, f)
    }

    pub(crate) fn write_buf_size(&self) -> usize {
        // check size for first level because delayed filter processing
        if let Some(wire) = &self.wire {
            self.app.write_len() + wire.write_len()
        } else {
            self.app.write_len()
        }
    }

    /// Returns the size of the transport-facing write buffer.
    pub(crate) fn write_dst_size(&self) -> usize {
        self.transport().write_len()
    }

    pub(crate) fn with_write_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.app.with_write(f)
    }

    pub(crate) fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.transport().with_write(f)
    }

    pub(crate) fn read_dst_size(&self) -> usize {
        self.app.read_len()
    }

    pub(crate) fn with_filter<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut FilterCtx<'_>) -> R,
    {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            stack: self,
            st: FilterUpdates { wants_write: false },
        };
        f(&mut ctx)
    }

    pub(crate) fn get_read_buf(&self) -> Option<BytesMut> {
        self.transport().read.take()
    }

    pub(crate) fn set_read_buf(&self, buf: BytesMut, cfg: &IoConfig) {
        let buffer = self.transport();
        if let Some(mut first_buf) = buffer.read.take() {
            // grow through the configured policy, so the merged buffer
            // stays cacheable when the data fits
            cfg.read_buf().resize_min(&mut first_buf, buf.len());
            first_buf.extend_from_slice(&buf);
            cfg.read_buf().release(buf);
            buffer.read.set(Some(first_buf));
        } else if !buf.is_empty() {
            buffer.read.set(Some(buf));
        } else {
            cfg.read_buf().release(buf);
        }
    }

    pub(crate) fn process_read_buf(&self, io: &IoRef) -> io::Result<FilterUpdates> {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            stack: self,
            st: FilterUpdates { wants_write: false },
        };
        io.with_callbacks(|cb| cb.before_processing(io));
        let result = io.filter().process_read_buf(&mut ctx);
        io.with_callbacks(|cb| cb.after_processing(io));

        result.map(|()| ctx.st)
    }

    pub(crate) fn process_read_buf_no_cb(&self, io: &IoRef) -> io::Result<FilterUpdates> {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            stack: self,
            st: FilterUpdates { wants_write: false },
        };
        io.filter().process_read_buf(&mut ctx).map(|()| ctx.st)
    }

    pub(crate) fn process_write_buf(&self, io: &IoRef) -> io::Result<()> {
        if self.app.is_write_empty() {
            Ok(())
        } else {
            let mut ctx = FilterCtx {
                io,
                idx: 0,
                stack: self,
                st: FilterUpdates { wants_write: true },
            };
            io.with_callbacks(|cb| cb.before_processing(io));
            let res = io.filter().process_write_buf(&mut ctx);
            io.with_callbacks(|cb| cb.after_processing(io));

            res
        }
    }

    pub(crate) fn process_write_buf_no_cb(&self, io: &IoRef) -> io::Result<()> {
        if self.app.is_write_empty() {
            Ok(())
        } else {
            let mut ctx = FilterCtx {
                io,
                idx: 0,
                stack: self,
                st: FilterUpdates { wants_write: true },
            };
            io.filter().process_write_buf(&mut ctx)
        }
    }

    pub(crate) fn process_write_buf_force(&self, io: &IoRef) -> io::Result<()> {
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            stack: self,
            st: FilterUpdates { wants_write: true },
        };
        io.with_callbacks(|cb| cb.before_processing(io));
        let res = io.filter().process_write_buf(&mut ctx);
        io.with_callbacks(|cb| cb.after_processing(io));

        res
    }

    pub(crate) fn process_shutdown(&self, io: &IoRef) -> io::Result<Poll<()>> {
        self.process_write_buf(io)?;
        io.with_callbacks(|cb| cb.before_processing(io));
        let res = self.with_filter(io, |ctx| io.filter().shutdown(ctx));
        io.with_callbacks(|cb| cb.after_processing(io));

        res
    }

    /// Releases the data of every buffer once nothing can consume it anymore.
    ///
    /// Read buffers go back to the cache and write pages are freed, the
    /// buffers themselves stay usable.
    pub(crate) fn release(&self, cfg: &IoConfig) {
        for b in self.buffers() {
            if let Some(buf) = b.read.take() {
                cfg.read_buf().release(buf);
            }
            b.with_write_if_free(BytePages::clear);
        }
    }
}

impl Buffer {
    fn new(size: BytePageSize) -> Self {
        Buffer {
            read: Cell::new(None),
            write: RefCell::new(BytePages::new(size)),
        }
    }

    /// Calls `f` unless the write buffer is in use.
    fn with_write_if_free(&self, f: impl FnOnce(&mut BytePages)) {
        if let Ok(mut wb) = self.write.try_borrow_mut() {
            f(&mut wb);
        }
    }

    fn is_write_empty(&self) -> bool {
        self.write.borrow().is_empty()
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
        self.write.borrow().len()
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

        #[cfg(debug_assertions)]
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
        f(&mut self.write.borrow_mut())
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct FilterUpdates {
    pub(crate) wants_write: bool,
}

#[derive(Debug)]
/// Context used while traversing a complete filter chain.
///
/// A context tracks the current layer and the write activity accumulated while
/// traversing it. [`with_next`](Self::with_next) advances to the inner layer,
/// while [`with_buffer`](Self::with_buffer) exposes the buffers adjacent to the
/// current layer.
pub struct FilterCtx<'a> {
    io: &'a IoRef,
    idx: usize,
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
    /// Invokes `f` with the context advanced to the next inner filter.
    ///
    /// The previous layer is restored after `f` returns.
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
    /// Invokes `f` with the buffers adjacent to the current filter.
    pub fn with_buffer<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut FilterBuf<'_>) -> R,
    {
        let mut buf = FilterBuf {
            io: self.io,
            curr: self.buffer(),
            next: self.stack.get(self.idx + 1),
            wants_write: Cell::new(self.st.wants_write),
        };
        let result = f(&mut buf);
        if buf.wants_write.get() {
            self.st.wants_write = true;
        }
        result
    }

    #[inline]
    /// Returns the size of the application-facing read buffer.
    pub fn read_dst_size(&self) -> usize {
        self.stack.app.read_len()
    }

    #[inline]
    /// Returns the size of the transport-facing write buffer.
    pub fn write_dst_size(&mut self) -> usize {
        self.stack.write_dst_size()
    }

    pub(crate) fn clear_write_buf(&mut self) {
        self.buffer().with_write(BytePages::clear);
    }

    fn buffer(&self) -> &Buffer {
        self.stack
            .get(self.idx)
            .expect("Filter context is outside of the filter chain")
    }
}

#[derive(Debug)]
/// Buffers and connection state adjacent to one [`FilterLayer`](crate::FilterLayer).
///
/// For reads, the source is transport-facing and the destination is
/// application-facing. For writes, the source is application-facing and the
/// destination is transport-facing. Buffers are returned to the chain after
/// each closure completes; empty read buffers may be returned to the
/// configured cache.
pub struct FilterBuf<'a> {
    io: &'a IoRef,
    curr: &'a Buffer,
    // `None` below the innermost layer, the transport uses `curr` directly
    next: Option<&'a Buffer>,
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

    /// Provides mutable access to the transport-facing read source.
    ///
    /// The source is optional because no bytes may currently be allocated for
    /// this edge of the filter chain. Leaving an empty buffer in the option
    /// returns it to the configured cache.
    pub fn with_read_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesMut>) -> R,
    {
        let mut read_src = self.next.and_then(|b| b.read.take());
        let result = f(&mut read_src);
        self.put_read_src(read_src);
        result
    }

    /// Provides the transport-facing read source and application-facing
    /// destination.
    ///
    /// Implementations normally consume bytes from `src` and append decoded or
    /// transformed bytes to `dst`. Unconsumed source bytes are retained for the
    /// next invocation.
    pub fn with_read_buffers<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Option<BytesMut>, &mut BytesMut) -> R,
    {
        let mut read_src = self.next.and_then(|b| b.read.take());
        let mut read_dst = self
            .curr
            .read
            .take()
            .unwrap_or_else(|| self.io.cfg().read_buf().get());

        let result = f(&mut read_src, &mut read_dst);

        self.put_read_src(read_src);
        if read_dst.is_empty() {
            self.io.cfg().read_buf().release(read_dst);
        } else {
            self.curr.read.set(Some(read_dst));
        }

        result
    }

    fn put_read_src(&self, src: Option<BytesMut>) {
        if let Some(b) = src {
            if b.is_empty() {
                self.io.cfg().read_buf().release(b);
            } else {
                // Without a filter layer there is no transport-facing read
                // source, the transport reads into the application-facing
                // buffer. Input stored here is never read.
                debug_assert!(
                    self.next.is_some(),
                    "{}: input stored in the read source of the innermost filter buffer is never read",
                    self.io.tag()
                );
                if let Some(next) = self.next {
                    next.read.set(Some(b));
                }
            }
        }
    }

    #[inline]
    /// Provides the application-facing write source and transport-facing
    /// destination.
    ///
    /// Implementations normally consume bytes from `src` and append encoded or
    /// transformed bytes to `dst`. Appending destination bytes marks the write
    /// chain as needing transport progress.
    pub fn with_write_buffers<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages, &mut BytePages) -> R,
    {
        let mut write_curr = self.curr.write.borrow_mut();
        let mut next = self.next.map(|b| b.write.borrow_mut());
        let mut on_demand;
        let write_next = if let Some(next) = next.as_deref_mut() {
            next
        } else {
            // Without a filter layer there is no transport-facing write
            // buffer, the transport writes from the application-facing one.
            // Output written here is never delivered.
            on_demand = BytePages::new(write_curr.page_size());
            &mut on_demand
        };
        let write_len = if self.wants_write.get() {
            0
        } else {
            write_next.len()
        };

        let result = f(&mut write_curr, write_next);

        if !self.wants_write.get() && write_next.len() > write_len {
            self.wants_write.set(true);
        }
        debug_assert!(
            self.next.is_some() || write_next.is_empty(),
            "{}: output written to the write destination of the innermost filter buffer is never sent",
            self.io.tag()
        );
        result
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let read = self.read.take();
        let write = self.write.try_borrow();

        let result = f
            .debug_struct("Buffer")
            .field("read", &read)
            .field("write", &write.as_deref().ok())
            .finish();
        self.read.set(read);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use ntex_bytes::BufMut;

    use super::*;
    use crate::{Io, testing::IoTest};

    #[ntex::test]
    async fn stack_without_layers() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();

        let stack = Stack::new(BytePageSize::Size8);
        assert_eq!(stack.layers(), 0);
        assert!(format!("{stack:?}").contains("layers: 0"));
        assert!(stack.mid.is_empty() && stack.wire.is_none());
        assert!(ptr::eq(stack.transport(), &raw const stack.app));
        assert!(stack.get(1).is_none());
        assert_eq!(stack.read_dst_size(), 0);
        assert_eq!(stack.write_buf_size(), 0);

        // the application and the transport share one buffer
        stack.with_write_src(|buf| buf.put_slice(b"out"));
        assert_eq!(stack.write_buf_size(), 3);
        assert_eq!(stack.write_dst_size(), 3);
        stack.set_read_buf(BytesMut::from(&b"one"[..]), ioref.cfg());
        stack.set_read_buf(BytesMut::from(&b"-two"[..]), ioref.cfg());
        assert_eq!(stack.read_dst_size(), 7);
        stack.with_read_dst(&ioref, |buf| assert_eq!(&buf[..], b"one-two"));

        // the base position has no inner buffer, nothing is kept for it
        stack.with_filter(&ioref, |ctx| {
            assert_eq!(ctx.read_dst_size(), 7);
            assert_eq!(ctx.write_dst_size(), 3);
            ctx.with_buffer(|buf| {
                assert!(ptr::eq(buf.curr, &raw const stack.app));
                assert!(buf.next.is_none());
                buf.with_read_buffers(|src, dst| {
                    assert!(src.is_none());
                    assert_eq!(&dst[..], b"one-two");
                });
                buf.with_read_src(|src| *src = Some(BytesMut::new()));
                buf.with_write_buffers(|src, dst| {
                    assert_eq!(src.len(), 3);
                    assert!(dst.is_empty());
                });
            });
        });
        assert!(stack.wire.is_none());

        assert_eq!(
            stack.with_write_dst(|buf| buf.split_to(3).freeze()),
            b"out".as_ref()
        );
        assert_eq!(stack.get_read_buf().as_deref(), Some(b"one-two".as_ref()));
        assert!(stack.get_read_buf().is_none());
        stack.set_read_buf(BytesMut::new(), ioref.cfg());
        assert!(stack.get_read_buf().is_none());

        stack.set_page_size(BytePageSize::Size32);
        stack
            .app
            .with_write(|buf| assert_eq!(buf.page_size(), BytePageSize::Size32));
    }

    #[ntex::test]
    async fn stack_with_one_layer() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();

        // data buffered before the layer is added belongs to the transport
        let mut stack = Stack::new(BytePageSize::Size8);
        stack.with_write_src(|buf| buf.put_slice(b"plain"));
        stack.set_read_buf(BytesMut::from(&b"hello"[..]), ioref.cfg());
        stack.add_layer(BytePageSize::Size16);

        assert_eq!(stack.layers(), 1);
        assert!(format!("{stack:?}").contains("layers: 1"));
        assert!(stack.mid.is_empty());
        let wire = stack.wire.as_ref().unwrap();
        assert!(ptr::eq(stack.transport(), wire));
        assert!(ptr::eq(stack.get(1).unwrap(), wire));
        assert!(stack.get(2).is_none());
        assert_eq!(stack.read_dst_size(), 0);
        assert_eq!(stack.write_dst_size(), 5);
        stack.app.with_write(|buf| {
            assert!(buf.is_empty());
            assert_eq!(buf.page_size(), BytePageSize::Size16);
        });

        // output of both sides is pending, filter processing may be delayed
        stack.with_write_src(|buf| buf.put_slice(b"app"));
        assert_eq!(stack.write_buf_size(), 8);

        stack.with_filter(&ioref, |ctx| {
            ctx.with_buffer(|buf| {
                assert!(ptr::eq(buf.curr, &raw const stack.app));
                buf.with_read_buffers(|src, dst| {
                    dst.extend_from_slice(&src.take().unwrap());
                });
                buf.with_write_buffers(|src, dst| {
                    assert_eq!(dst.len(), 5);
                    src.move_to(dst);
                });
            });
            ctx.with_next(|ctx| {
                ctx.with_buffer(|buf| {
                    assert!(ptr::eq(buf.curr, wire));
                    assert!(buf.next.is_none());
                });
            });
        });

        assert_eq!(stack.read_dst_size(), 5);
        assert!(stack.get_read_buf().is_none());
        stack.with_read_dst(&ioref, |buf| assert_eq!(&buf[..], b"hello"));
        assert_eq!(stack.write_buf_size(), 8);
        assert_eq!(
            stack.with_write_dst(|buf| buf.split_to(8).freeze()),
            b"plainapp".as_ref()
        );
        assert_eq!(stack.write_buf_size(), 0);
    }

    #[ntex::test]
    async fn stack_with_more_layers() {
        type Seen = (Option<Vec<u8>>, Vec<u8>, usize, usize);

        fn visit(ctx: &mut FilterCtx<'_>, seen: &mut Vec<Seen>) {
            ctx.with_buffer(|buf| {
                let (src, dst) = buf.with_read_buffers(|src, dst| {
                    (src.as_deref().map(<[u8]>::to_vec), dst.to_vec())
                });
                let (wsrc, wdst) = buf.with_write_buffers(|src, dst| (src.len(), dst.len()));
                seen.push((src, dst, wsrc, wdst));
            });
            if seen.len() < 4 {
                ctx.with_next(|ctx| visit(ctx, seen));
            }
        }

        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();

        // each layer is added outermost, buffered data moves toward the
        // transport, the data of the first layer ends up in `wire`
        let mut stack = Stack::new(BytePageSize::Size8);
        for data in ["a", "bb", "cccc"] {
            stack.with_write_src(|buf| buf.put_slice(data.as_bytes()));
            stack.app.read.set(Some(BytesMut::from(data.as_bytes())));
            stack.add_layer(BytePageSize::Size16);
        }
        assert_eq!(stack.layers(), 3);
        assert!(format!("{stack:?}").contains("layers: 3"));
        assert_eq!(stack.mid.len(), 2);
        assert!(ptr::eq(stack.transport(), stack.wire.as_ref().unwrap()));
        assert!(stack.get(4).is_none());

        // only the outermost and the transport-facing buffers are pending
        stack.with_write_src(|buf| buf.put_slice(b"app"));
        assert_eq!(stack.write_buf_size(), 4);
        assert_eq!(stack.write_dst_size(), 1);
        assert_eq!(stack.read_dst_size(), 0);

        let mut seen = Vec::new();
        stack.with_filter(&ioref, |ctx| visit(ctx, &mut seen));
        assert_eq!(
            seen,
            [
                (Some(b"cccc".to_vec()), Vec::new(), 3, 4),
                (Some(b"bb".to_vec()), b"cccc".to_vec(), 4, 2),
                (Some(b"a".to_vec()), b"bb".to_vec(), 2, 1),
                (None, b"a".to_vec(), 1, 0),
            ]
        );
        assert_eq!(stack.get_read_buf().as_deref(), Some(b"a".as_ref()));

        stack.set_page_size(BytePageSize::Size32);
        for buf in stack.buffers() {
            buf.with_write(|buf| assert_eq!(buf.page_size(), BytePageSize::Size32));
        }

        stack.release(ioref.cfg());
        assert_eq!(stack.buffers().count(), 4);
        for buf in stack.buffers() {
            assert_eq!(buf.read_len(), 0);
            assert_eq!(buf.write_len(), 0);
        }
    }

    #[ntex::test]
    #[should_panic(expected = "outside of the filter chain")]
    async fn filter_ctx_outside_of_chain() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let mut stack = Stack::new(BytePageSize::Size8);
        stack.add_layer(BytePageSize::Size8);

        stack.with_filter(&ioref, |ctx| {
            ctx.with_next(|ctx| ctx.with_next(|ctx| ctx.with_buffer(|_| ())));
        });
    }

    #[ntex::test]
    async fn set_read_buf_merges_into_cacheable_buffer() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let cfg = ioref.cfg().read_buf();
        let stack = Stack::new(BytePageSize::Size8);

        // unconsumed input, most of the buffer is taken by a decoded frame
        // that is still alive
        let mut first = cfg.get();
        first.extend_from_slice(&vec![1; cfg.high - 100]);
        let frame = first.split_to(cfg.high - 1100);
        stack.set_read_buf(first, ioref.cfg());

        // a read into a buffer of its own completes
        let mut second = cfg.get();
        second.extend_from_slice(&[2; 4000]);
        stack.set_read_buf(second, ioref.cfg());

        let merged = stack.get_read_buf().unwrap();
        assert_eq!(merged.len(), 5000);
        assert_eq!(&merged[..1000], &[1; 1000][..]);
        assert_eq!(&merged[1000..], &[2; 4000][..]);
        assert_eq!(merged.capacity(), cfg.high);
        assert_eq!(frame.len(), cfg.high - 1100);
    }

    #[ntex::test]
    async fn filter_read_buffers() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let mut stack = Stack::new(BytePageSize::Size8);
        stack.add_layer(BytePageSize::Size8);
        stack.set_read_buf(BytesMut::from(&b"input"[..]), ioref.cfg());

        stack.with_filter(&ioref, |ctx| {
            assert_eq!(ctx.io(), &ioref);
            assert_eq!(ctx.tag(), ioref.tag());
            assert_eq!(ctx.read_dst_size(), 0);

            ctx.with_buffer(|buf| {
                assert_eq!(buf.io(), &ioref);
                assert_eq!(buf.tag(), ioref.tag());
                buf.with_read_buffers(|src, dst| {
                    let src = src.as_mut().unwrap();
                    dst.extend_from_slice(&src.split_to(2));
                });
            });
        });

        assert_eq!(stack.read_dst_size(), 2);
        stack.with_read_dst(&ioref, |buf| assert_eq!(&buf[..], b"in"));
        assert_eq!(stack.get_read_buf().as_deref(), Some(b"put".as_ref()));

        stack.with_filter(&ioref, |ctx| {
            ctx.with_buffer(|buf| {
                buf.with_read_src(|src| {
                    *src = Some(BytesMut::from(&b"next"[..]));
                });
            });
        });
        assert_eq!(stack.get_read_buf().as_deref(), Some(b"next".as_ref()));
    }

    #[cfg(debug_assertions)]
    #[ntex::test]
    #[should_panic(expected = "is never sent")]
    async fn innermost_write_destination_output_asserts() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let stack = Stack::new(BytePageSize::Size8);

        stack.with_write_src(|buf| buf.put_slice(b"out"));
        stack.with_filter(&ioref, |ctx| {
            ctx.with_buffer(|buf| buf.with_write_buffers(BytePages::move_to));
        });
    }

    #[cfg(debug_assertions)]
    #[ntex::test]
    #[should_panic(expected = "is never read")]
    async fn innermost_read_source_input_asserts() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let stack = Stack::new(BytePageSize::Size8);

        stack.with_filter(&ioref, |ctx| {
            ctx.with_buffer(|buf| {
                buf.with_read_src(|src| *src = Some(BytesMut::from(&b"in"[..])));
            });
        });
    }

    #[ntex::test]
    async fn filter_write_buffers_and_updates() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let mut stack = Stack::new(BytePageSize::Size8);
        stack.add_layer(BytePageSize::Size8);
        stack.with_write_src(|buf| buf.put_slice(b"output"));

        let updates = stack.with_filter(&ioref, |ctx| {
            assert_eq!(ctx.write_dst_size(), 0);
            ctx.with_buffer(|buf| {
                buf.with_write_buffers(|src, dst| {
                    assert_eq!(src.len(), 6);
                    assert_eq!(dst.len(), 0);
                    src.move_to(dst);
                });
            });
            ctx.st
        });

        assert!(updates.wants_write);
        assert_eq!(stack.write_buf_size(), 6);
        assert_eq!(
            stack.with_write_dst(|buf| buf.split_to(6).freeze()),
            b"output".as_ref()
        );
        assert_eq!(stack.write_buf_size(), 0);
    }

    #[ntex::test]
    async fn buffer_debug_preserves_contents() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let buffer = Buffer::new(BytePageSize::Size8);

        buffer.with_read(&ioref, |buf| buf.extend_from_slice(b"read"));
        buffer.with_write(|buf| buf.put_slice(b"write"));

        let debug = format!("{buffer:?}");
        assert!(debug.contains("Buffer"));
        assert_eq!(buffer.read_len(), 4);
        assert_eq!(buffer.write_len(), 5);

        // a write buffer in use is not shown
        let debug = buffer.with_write(|_| format!("{buffer:?}"));
        assert!(debug.contains("write: None"));
        assert_eq!(buffer.read_len(), 4);
    }
}
