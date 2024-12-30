use crate::{Bytes, BytesMut, BytesVec};
use std::fmt::{Formatter, LowerHex, Result, UpperHex};

struct BytesRef<'a>(&'a [u8]);

impl LowerHex for BytesRef<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl UpperHex for BytesRef<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        for b in self.0 {
            write!(f, "{b:02X}")?;
        }
        Ok(())
    }
}

macro_rules! hex_impl {
    ($tr:ident, $ty:ty) => {
        impl $tr for $ty {
            fn fmt(&self, f: &mut Formatter<'_>) -> Result {
                $tr::fmt(&BytesRef(self.as_ref()), f)
            }
        }
    };
}

hex_impl!(LowerHex, Bytes);
hex_impl!(LowerHex, BytesMut);
hex_impl!(LowerHex, BytesVec);
hex_impl!(UpperHex, Bytes);
hex_impl!(UpperHex, BytesMut);
hex_impl!(UpperHex, BytesVec);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex() {
        let b = Bytes::from_static(b"hello world");
        let f = format!("{:x}", b);
        assert_eq!(f, "68656c6c6f20776f726c64");
        let f = format!("{:X}", b);
        assert_eq!(f, "68656C6C6F20776F726C64");
    }
}
