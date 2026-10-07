use std::{cell::Cell, cell::Ref, cell::RefCell, fmt, io, iter, mem, task::Poll};

use ntex_bytes::{BytePageSize, BytePages, BytesMut};

use crate::IoRef;

/// Buffers of the filter chain, ordered from the application toward the
/// transport.
///
/// Without a filter layer the application and the transport share `app`.
/// Each added layer becomes the outermost one: it gets a new `app` buffer, and
/// the previous `app` buffer, with any data buffered in it, moves one position
/// toward the transport. The first one becomes `wire`, further ones go to
/// `mid`. The layer at position `i` uses the buffers at positions `i` and
/// `i + 1`, the innermost layer writes to and reads from `wire`.
///
/// Adding a layer moves buffers, so it needs exclusive access to the layers.
/// Everything that holds references into them, including while it runs code
/// it does not control such as closures, codecs or filters, holds a shared
/// borrow.
pub(crate) struct Stack(RefCell<Layers>);

pub(crate) struct Layers {
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
            .field("layers", &self.borrow().count())
            .finish()
    }
}

impl fmt::Debug for Layers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Layers")
            .field("layers", &self.count())
            .finish()
    }
}

impl Layers {
    /// Returns the number of installed filter layers.
    fn count(&self) -> usize {
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

    /// Returns the size of the transport-facing write buffer.
    fn write_dst_size(&self) -> usize {
        self.transport().write_len()
    }
}

impl Stack {
    pub(crate) fn new(size: BytePageSize) -> Self {
        Self(RefCell::new(Layers {
            app: Buffer::new(size),
            mid: Vec::new(),
            wire: None,
        }))
    }

    /// Borrows the layers, the stack cannot change until the borrow is
    /// dropped.
    #[inline]
    pub(crate) fn borrow(&self) -> Ref<'_, Layers> {
        self.0.borrow()
    }

    /// Whether references into the stack may be held by running code.
    pub(crate) fn is_borrowed(&self) -> bool {
        self.0.try_borrow_mut().is_err()
    }

    pub(crate) fn set_page_size(&self, size: BytePageSize) {
        for b in self.borrow().buffers() {
            b.with_write_if_free(|b| b.set_page_size(size));
        }
    }

    /// Adds a buffer for a new outermost layer.
    ///
    /// # Panics
    ///
    /// Panics if the stack is borrowed.
    pub(crate) fn add_layer(&self, page_size: BytePageSize) {
        let Ok(mut layers) = self.0.try_borrow_mut() else {
            panic!("filter buffers are in use");
        };
        let outer = mem::replace(&mut layers.app, Buffer::new(page_size));
        if layers.wire.is_none() {
            layers.wire = Some(outer);
        } else {
            layers.mid.insert(0, outer);
        }
    }

    pub(crate) fn with_read_src<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.borrow().transport().with_read(io, f)
    }

    pub(crate) fn with_read_dst<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.borrow().app.with_read(io, f)
    }

    pub(crate) fn write_buf_size(&self) -> usize {
        // check size for first level because delayed filter processing
        let layers = self.borrow();
        if let Some(wire) = &layers.wire {
            layers.app.write_len() + wire.write_len()
        } else {
            layers.app.write_len()
        }
    }

    /// Returns the size of the transport-facing write buffer.
    pub(crate) fn write_dst_size(&self) -> usize {
        self.borrow().write_dst_size()
    }

    pub(crate) fn with_write_src<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.borrow().app.with_write(f)
    }

    pub(crate) fn with_write_dst<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut BytePages) -> R,
    {
        self.borrow().transport().with_write(f)
    }

    pub(crate) fn read_dst_size(&self) -> usize {
        self.borrow().app.read_len()
    }

    pub(crate) fn with_filter<F, R>(&self, io: &IoRef, f: F) -> R
    where
        F: FnOnce(&mut FilterCtx<'_>) -> R,
    {
        let layers = self.borrow();
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            layers: &layers,
            st: FilterUpdates { wants_write: false },
        };
        f(&mut ctx)
    }

    pub(crate) fn get_read_buf(&self) -> Option<BytesMut> {
        self.borrow().transport().read.take()
    }

    pub(crate) fn set_read_buf(&self, buf: BytesMut) {
        let layers = self.borrow();
        let buffer = layers.transport();
        if let Some(mut first_buf) = buffer.read.take() {
            first_buf.extend_from_slice(&buf);
            buffer.read.set(Some(first_buf));
        } else if !buf.is_empty() {
            buffer.read.set(Some(buf));
        }
    }

    pub(crate) fn process_read_buf(&self, io: &IoRef) -> io::Result<FilterUpdates> {
        let layers = self.borrow();
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            layers: &layers,
            st: FilterUpdates { wants_write: false },
        };
        io.with_callbacks(|cb| cb.before_processing(io));
        let result = io.filter().process_read_buf(&mut ctx);
        io.with_callbacks(|cb| cb.after_processing(io));

        result.map(|()| ctx.st)
    }

    pub(crate) fn process_read_buf_no_cb(&self, io: &IoRef) -> io::Result<FilterUpdates> {
        let layers = self.borrow();
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            layers: &layers,
            st: FilterUpdates { wants_write: false },
        };
        io.filter().process_read_buf(&mut ctx).map(|()| ctx.st)
    }

    pub(crate) fn process_write_buf(&self, io: &IoRef) -> io::Result<()> {
        let layers = self.borrow();
        if layers.app.is_write_empty() {
            Ok(())
        } else {
            let mut ctx = FilterCtx {
                io,
                idx: 0,
                layers: &layers,
                st: FilterUpdates { wants_write: true },
            };
            io.with_callbacks(|cb| cb.before_processing(io));
            let res = io.filter().process_write_buf(&mut ctx);
            io.with_callbacks(|cb| cb.after_processing(io));

            res
        }
    }

    pub(crate) fn process_write_buf_no_cb(&self, io: &IoRef) -> io::Result<()> {
        let layers = self.borrow();
        if layers.app.is_write_empty() {
            Ok(())
        } else {
            let mut ctx = FilterCtx {
                io,
                idx: 0,
                layers: &layers,
                st: FilterUpdates { wants_write: true },
            };
            io.filter().process_write_buf(&mut ctx)
        }
    }

    pub(crate) fn process_write_buf_force(&self, io: &IoRef) -> io::Result<()> {
        let layers = self.borrow();
        let mut ctx = FilterCtx {
            io,
            idx: 0,
            layers: &layers,
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
    pub(crate) fn release(&self) {
        for b in self.borrow().buffers() {
            drop(b.read.take());
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
        let mut rb = self.read.take().unwrap_or_else(|| io.0.get_read_buf());
        let result = f(&mut rb);

        #[cfg(debug_assertions)]
        // check nested updates
        if self.read.take().is_some() {
            log::error!("Nested read io operation is detected");
            io.terminate();
        }

        if !rb.is_empty() {
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
    layers: &'a Layers,
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
            next: self.layers.get(self.idx + 1),
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
        self.layers.app.read_len()
    }

    #[inline]
    /// Returns the size of the transport-facing write buffer.
    pub fn write_dst_size(&mut self) -> usize {
        self.layers.write_dst_size()
    }

    pub(crate) fn clear_write_buf(&mut self) {
        self.buffer().with_write(BytePages::clear);
    }

    fn buffer(&self) -> &Buffer {
        self.layers
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
            .unwrap_or_else(|| self.io.0.get_read_buf());

        let result = f(&mut read_src, &mut read_dst);

        self.put_read_src(read_src);
        if !read_dst.is_empty() {
            self.curr.read.set(Some(read_dst));
        }

        result
    }

    fn put_read_src(&self, src: Option<BytesMut>) {
        if let Some(b) = src
            && !b.is_empty()
        {
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

    #[test]
    fn miri_add_layer_keeps_buffers() {
        let stack = Stack::new(BytePageSize::Size8);
        for i in 0..4u8 {
            let layers = stack.borrow();
            let nested = stack.borrow();
            let mut buf = BytesMut::new();
            buf.extend_from_slice(&[i]);
            layers.app.read.set(Some(buf));
            drop(nested);
            assert!(stack.is_borrowed());
            drop(layers);
            stack.add_layer(BytePageSize::Size8);
        }

        let layers = stack.borrow();
        assert_eq!(layers.count(), 4);
        let reads: Vec<_> = layers
            .buffers()
            .map(|b| b.read.take().map(|b| b.to_vec()))
            .collect();
        assert_eq!(
            reads,
            [
                None,
                Some(vec![3]),
                Some(vec![2]),
                Some(vec![1]),
                Some(vec![0])
            ]
        );
    }

    #[test]
    #[should_panic(expected = "filter buffers are in use")]
    fn miri_add_layer_while_borrowed() {
        let stack = Stack::new(BytePageSize::Size8);
        let _layers = stack.borrow();
        stack.add_layer(BytePageSize::Size8);
    }

    #[ntex::test]
    async fn stack_without_layers() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();

        let stack = Stack::new(BytePageSize::Size8);
        let layers = stack.borrow();
        assert_eq!(layers.count(), 0);
        assert!(format!("{stack:?}").contains("layers: 0"));
        assert!(layers.mid.is_empty() && layers.wire.is_none());
        assert!(ptr::eq(layers.transport(), &raw const layers.app));
        assert!(layers.get(1).is_none());
        assert_eq!(stack.read_dst_size(), 0);
        assert_eq!(stack.write_buf_size(), 0);

        // the application and the transport share one buffer
        stack.with_write_src(|buf| buf.put_slice(b"out"));
        assert_eq!(stack.write_buf_size(), 3);
        assert_eq!(stack.write_dst_size(), 3);
        stack.set_read_buf(BytesMut::from(&b"one"[..]));
        stack.set_read_buf(BytesMut::from(&b"-two"[..]));
        assert_eq!(stack.read_dst_size(), 7);
        stack.with_read_dst(&ioref, |buf| assert_eq!(&buf[..], b"one-two"));

        // the base position has no inner buffer, nothing is kept for it
        stack.with_filter(&ioref, |ctx| {
            assert_eq!(ctx.read_dst_size(), 7);
            assert_eq!(ctx.write_dst_size(), 3);
            ctx.with_buffer(|buf| {
                assert!(ptr::eq(buf.curr, &raw const layers.app));
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
        assert!(layers.wire.is_none());

        assert_eq!(
            stack.with_write_dst(|buf| buf.split_to(3).freeze()),
            b"out".as_ref()
        );
        assert_eq!(stack.get_read_buf().as_deref(), Some(b"one-two".as_ref()));
        assert!(stack.get_read_buf().is_none());
        stack.set_read_buf(BytesMut::new());
        assert!(stack.get_read_buf().is_none());

        stack.set_page_size(BytePageSize::Size32);
        layers
            .app
            .with_write(|buf| assert_eq!(buf.page_size(), BytePageSize::Size32));
    }

    #[ntex::test]
    async fn stack_with_one_layer() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();

        // data buffered before the layer is added belongs to the transport
        let stack = Stack::new(BytePageSize::Size8);
        stack.with_write_src(|buf| buf.put_slice(b"plain"));
        stack.set_read_buf(BytesMut::from(&b"hello"[..]));
        stack.add_layer(BytePageSize::Size16);

        let layers = stack.borrow();
        assert_eq!(layers.count(), 1);
        assert!(format!("{stack:?}").contains("layers: 1"));
        assert!(layers.mid.is_empty());
        let wire = layers.wire.as_ref().unwrap();
        assert!(ptr::eq(layers.transport(), wire));
        assert!(ptr::eq(layers.get(1).unwrap(), wire));
        assert!(layers.get(2).is_none());
        assert_eq!(stack.read_dst_size(), 0);
        assert_eq!(stack.write_dst_size(), 5);
        layers.app.with_write(|buf| {
            assert!(buf.is_empty());
            assert_eq!(buf.page_size(), BytePageSize::Size16);
        });

        // output of both sides is pending, filter processing may be delayed
        stack.with_write_src(|buf| buf.put_slice(b"app"));
        assert_eq!(stack.write_buf_size(), 8);

        stack.with_filter(&ioref, |ctx| {
            ctx.with_buffer(|buf| {
                assert!(ptr::eq(buf.curr, &raw const layers.app));
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
        let stack = Stack::new(BytePageSize::Size8);
        for data in ["a", "bb", "cccc"] {
            stack.with_write_src(|buf| buf.put_slice(data.as_bytes()));
            stack
                .borrow()
                .app
                .read
                .set(Some(BytesMut::from(data.as_bytes())));
            stack.add_layer(BytePageSize::Size16);
        }
        let layers = stack.borrow();
        assert_eq!(layers.count(), 3);
        assert!(format!("{stack:?}").contains("layers: 3"));
        assert_eq!(layers.mid.len(), 2);
        assert!(ptr::eq(layers.transport(), layers.wire.as_ref().unwrap()));
        assert!(layers.get(4).is_none());

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
        for buf in layers.buffers() {
            buf.with_write(|buf| assert_eq!(buf.page_size(), BytePageSize::Size32));
        }

        stack.release();
        assert_eq!(layers.buffers().count(), 4);
        for buf in layers.buffers() {
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
        let stack = Stack::new(BytePageSize::Size8);
        stack.add_layer(BytePageSize::Size8);

        stack.with_filter(&ioref, |ctx| {
            ctx.with_next(|ctx| ctx.with_next(|ctx| ctx.with_buffer(|_| ())));
        });
    }

    #[ntex::test]
    async fn set_read_buf_merges_into_cacheable_buffer() {
        let high = BytePageSize::Size16.capacity();
        let stack = Stack::new(BytePageSize::Size8);

        // unconsumed input, most of the buffer is taken by a decoded frame
        // that is still alive
        let mut first = BytesMut::with_page_size(BytePageSize::Size16);
        first.extend_from_slice(&vec![1; high - 100]);
        let frame = first.split_to(high - 1100);
        stack.set_read_buf(first);

        // a read into a buffer of its own completes
        let mut second = BytesMut::with_page_size(BytePageSize::Size16);
        second.extend_from_slice(&[2; 4000]);
        stack.set_read_buf(second);

        let merged = stack.get_read_buf().unwrap();
        assert_eq!(merged.len(), 5000);
        assert_eq!(&merged[..1000], &[1; 1000][..]);
        assert_eq!(&merged[1000..], &[2; 4000][..]);
        assert_eq!(merged.capacity(), high);
        assert_eq!(frame.len(), high - 1100);
    }

    #[ntex::test]
    async fn filter_read_buffers() {
        let (_, server) = IoTest::create();
        let io = Io::from(server);
        let ioref = io.get_ref();
        let stack = Stack::new(BytePageSize::Size8);
        stack.add_layer(BytePageSize::Size8);
        stack.set_read_buf(BytesMut::from(&b"input"[..]));

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
        let stack = Stack::new(BytePageSize::Size8);
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
