use bytes::BytesMut;

/// Trait of helper objects to write out messages as bytes.
pub trait Encoder {
    /// The type of items consumed by the `Encoder`
    type Item;

    /// The type of encoding errors.
    type Error;

    /// Encodes a frame into the buffer provided.
    fn encode(
        &mut self,
        item: Self::Item,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error>;
}
