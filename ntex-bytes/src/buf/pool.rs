use bytes::{Buf, BufMut, Bytes, BytesMut, buf::UninitSlice};
use std::future::{self, Future};
use std::io::IoSlice;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

/// A source of buffers.
///
/// Dropped buffers will automatically return to this pool.
pub trait BufferSource {
    /// The type of buffer returned by this buffer source that owns its contents.
    type Owned: Owned;

    /// The type of accumulator of buffers used by encoders.
    type Accumulator: BufferAccumulator<Shared = <Self::Owned as Owned>::Shared>;

    /// The type of future returned by the [`BufferSource::take`] impl.
    type TakeFuture: Future<Output = Self::Owned>;

    /// Asynchronously returns a buffer that is at least as long as the requested length.
    fn take(&self, len: usize) -> Self::TakeFuture;
}

impl<T> BufferSource for &'_ T where T: BufferSource {
    type Owned = T::Owned;
    type Accumulator = T::Accumulator;
    type TakeFuture = T::TakeFuture;

    fn take(&self, len: usize) -> Self::TakeFuture {
        (**self).take(len)
    }
}

/// A sink of buffers. Dropped buffers will automatically return to this sink.
pub trait BufferSink {
    /// The type of buffer returned by this buffer source that owns its contents.
    type Owned: Owned;

    /// The type of backing buffer that is returned to this pool when the last owned or shared buffer
    /// holding on to said backing buffer is dropped.
    type Backing;

    /// Returns the given backing buffer to the pool. This is intended to be invoked from
    /// the `Drop` impls of this pool's owned and shared buffer types.
    fn put_back(&self, backing: Self::Backing);
}

/// Convenience trait for a type that is a buffer pool, ie a type that is both a [`BufferSource`]
/// and a [`BufferSink`].
pub trait BufferPool: BufferSource + BufferSink<Owned = <Self as BufferSource>::Owned> {}

impl<T> BufferPool for T where T: BufferSource + BufferSink<Owned = <Self as BufferSource>::Owned> {}

impl<T> BufferSink for Rc<T>
where
    T: ?Sized + BufferSink,
{
    type Backing = T::Backing;
    type Owned = T::Owned;

    fn put_back(&self, backing: Self::Backing) {
        (&**self).put_back(backing);
    }
}

impl<T> BufferSink for Arc<T>
where
    T: BufferSink,
{
    type Backing = T::Backing;
    type Owned = T::Owned;

    fn put_back(&self, backing: Self::Backing) {
        (&**self).put_back(backing);
    }
}

/// A [`BufferSource`] that always yields new `BytesMut` without blocking.
#[derive(Clone, Copy, Debug)]
pub struct BytesPool;

impl BufferSource for BytesPool {
    type Owned = BytesMut;
    type Accumulator = BytesAccumulator;
    type TakeFuture = future::Ready<Self::Owned>;

    fn take(&self, len: usize) -> Self::TakeFuture {
        future::ready(BytesMut::with_capacity(len))
    }
}

impl BufferSink for BytesPool {
    type Backing = ();
    type Owned = <Self as BufferSource>::Owned;

    fn put_back(&self, _backing: Self::Backing) {
    }
}

/// A buffer that owns a particular range of its backing buffer.
///
/// An `Owned` tracks what part of itself has been filled with data.
/// The filled region can be accessed with [`Owned::filled`].
/// The unfilled region can be accessed with [`Owned::unfilled_mut`], and bytes can be moved from the start of this region
/// into the end of the filled region with [`Owned::fill`].
///
/// An `Owned` can be subdivided into smaller `Owned`s with `split_to` that each own
/// smaller splits of the backing buffer.
///
/// An `Owned` is not `Clone`. It can be converted to a [`Shared`] which is, via [`Owned::freeze`]
pub trait Owned {
    /// The type of shared buffers with the same backing buffer.
    type Shared: Shared;

    /// Try to append a `u8` to this buffer's filled region.
    ///
    /// Returns `None` if there is no space in the filled region to append this `u8`.
    fn try_put_u8(&mut self, n: u8) -> Option<()> {
        self.try_put_slice(&[n])
    }

    /// Try to append a `u16` in big-endian order to this buffer's filled region.
    ///
    /// Returns `None` if there is no space in the filled region to append this `u16`.
    fn try_put_u16_be(&mut self, n: u16) -> Option<()> {
        self.try_put_slice(&n.to_be_bytes())
    }

    /// Try to append a `u32` in big-endian order to this buffer's filled region.
    ///
    /// Returns `None` if there is no space in the filled region to append this `u32`.
    fn try_put_u32_be(&mut self, n: u32) -> Option<()> {
        self.try_put_slice(&n.to_be_bytes())
    }

    /// Try to append a slice in big-endian order to this buffer's filled region.
    ///
    /// Returns `None` if there is no space in the filled region to append this slice.
    fn try_put_slice(&mut self, src: &[u8]) -> Option<()>;

    /// Returns the filled region of this buffer.
    fn filled(&self) -> &[u8];

    /// Returns whether the filled region of this buffer is empty or not.
    fn filled_is_empty(&self) -> bool {
        self.filled().is_empty()
    }

    /// Retains the range `i..` in `self`, and returns a new `Owned` for the range `0..i`
    fn split_to(&mut self, i: usize) -> Self;

    /// Returns the unfilled region of this buffer.
    fn unfilled_mut(&mut self) -> &mut UninitSlice;

    /// Moves the given number of bytes from the start of the unfilled region to the end of the filled region.
    unsafe fn fill(&mut self, n: usize);
}

/// A buffer that has a shared reference to a particular range of its backing buffer.
pub trait Shared: AsRef<[u8]> + Deref<Target = [u8]> {
}

impl Owned for BytesMut {
    type Shared = Bytes;

    fn try_put_slice(&mut self, src: &[u8]) -> Option<()> {
        self.extend_from_slice(src);
        Some(())
    }

    fn filled(&self) -> &[u8] {
        Buf::chunk(self)
    }

    fn split_to(&mut self, i: usize) -> Self {
        self.split_to(i)
    }

    fn unfilled_mut(&mut self) -> &mut UninitSlice {
        BufMut::chunk_mut(self)
    }

    /// Moves the given number of bytes from the start of the unfilled region to the end of the filled region.
    unsafe fn fill(&mut self, n: usize) {
        BufMut::advance_mut(self, n);
    }
}

impl Shared for Bytes {
}

/// An accumulator that collects buffers.
///
/// This is used by encoders that need to make a collection of smaller chunks out of each item they wish to encode.
pub trait BufferAccumulator {
    /// The type of shared buffers that can be appended to this accumulator.
    type Shared: Shared;

    /// Constructs a new, empty accumulator with the specified capacity (in bytes).
    fn with_capacity(capacity: usize) -> Self;

    /// Append a `Self::Shared` to this accumulator.
    ///
    /// Returns `None` if appending fails for any reason.
    fn put_bytes(&mut self, src: Self::Shared) -> Option<()>;

    /// Returns whether this accumulator has no bytes in it or not.
    fn is_empty(&self) -> bool;

    /// Returns whether this accumulator can accept more bytes or not.
    fn is_full(&self) -> bool;

    /// Returns the first (and possibly only) chunk from this accumulator.
    ///
    /// Returns `None` if this accumulator is empty.
    fn chunk(&self) -> &[u8];

    /// Fills as many chunks from this accumulator as possible into the given [`IoSlice`]s slice.
    ///
    /// Returns the number of chunks set to valid data in `dst`.
    fn chunks_vectored<'a>(&'a self, dst: &mut [IoSlice<'a>]) -> usize;

    /// Drops `cnt` bytes from the front of this accumulator.
    fn advance(&mut self, cnt: usize);
}

/// An [`Accumulator`] around an inner [`BytesMut`].
#[derive(Debug)]
pub struct BytesAccumulator {
    inner: BytesMut,
    initial_capacity: usize,
}

impl BufferAccumulator for BytesAccumulator {
    type Shared = Bytes;

    fn with_capacity(capacity: usize) -> Self {
        BytesAccumulator {
            inner: BytesMut::with_capacity(capacity),
            initial_capacity: capacity,
        }
    }

    fn put_bytes(&mut self, src: Self::Shared) -> Option<()> {
        self.inner.extend_from_slice(&src);
        Some(())
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn is_full(&self) -> bool {
        self.inner.len() >= self.initial_capacity
    }

    fn chunk(&self) -> &[u8] {
        Buf::chunk(&self.inner)
    }

    fn chunks_vectored<'a>(&'a self, dst: &mut [IoSlice<'a>]) -> usize {
        Buf::chunks_vectored(&self.inner, dst)
    }

    fn advance(&mut self, cnt: usize) {
        Buf::advance(&mut self.inner, cnt);
    }
}
