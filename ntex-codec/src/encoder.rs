use ntex_bytes::BytesMut;
use std::rc::Rc;

/// Trait of helper objects to write out messages as bytes.
pub trait Encoder {
    /// The type of items consumed by the `Encoder`
    type Item;

    /// The type of encoding errors.
    type Error: std::fmt::Debug;

    /// Encodes a frame into the buffer provided.
    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error>;
}

impl<T> Encoder for Rc<T>
where
    T: Encoder,
{
    type Item = T::Item;
    type Error = T::Error;

    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        (**self).encode(item, dst)
    }
}
