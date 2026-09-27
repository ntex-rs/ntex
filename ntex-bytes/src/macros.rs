//! Trait impls shared by `Bytes`, `BytesMut` and `BytePage`. `impl_buf!` and
//! `impl_slice_traits!` expect a `storage` field and a `new()` constructor,
//! `impl_buf!` and `impl_read!` expect `len()` and `advance_to()`.

/// Implements `ntex_bytes::Buf` and `bytes::Buf`, `extra` items go to both.
macro_rules! impl_buf {
    ($ty:ty { $($extra:tt)* }) => {
        impl $crate::Buf for $ty {
            #[inline]
            fn remaining(&self) -> usize {
                self.len()
            }

            #[inline]
            fn chunk(&self) -> &[u8] {
                self.storage.as_ref()
            }

            #[inline]
            fn advance(&mut self, cnt: usize) {
                self.advance_to(cnt);
            }

            $($extra)*
        }

        impl ::bytes::buf::Buf for $ty {
            #[inline]
            fn remaining(&self) -> usize {
                self.len()
            }

            #[inline]
            fn chunk(&self) -> &[u8] {
                self.storage.as_ref()
            }

            #[inline]
            fn advance(&mut self, cnt: usize) {
                self.advance_to(cnt);
            }

            $($extra)*
        }
    };
}

/// Implements the traits that expose the data as a byte slice.
macro_rules! impl_slice_traits {
    ($ty:ident) => {
        impl AsRef<[u8]> for $ty {
            #[inline]
            fn as_ref(&self) -> &[u8] {
                self.storage.as_ref()
            }
        }

        impl ::std::ops::Deref for $ty {
            type Target = [u8];

            #[inline]
            fn deref(&self) -> &[u8] {
                self.storage.as_ref()
            }
        }

        impl ::std::borrow::Borrow<[u8]> for $ty {
            #[inline]
            fn borrow(&self) -> &[u8] {
                self.storage.as_ref()
            }
        }

        impl Default for $ty {
            #[inline]
            fn default() -> $ty {
                $ty::new()
            }
        }

        impl ::std::fmt::Debug for $ty {
            fn fmt(&self, fmt: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                ::std::fmt::Debug::fmt(&$crate::debug::BsDebug(self.storage.as_ref()), fmt)
            }
        }

        impl IntoIterator for $ty {
            type Item = u8;
            type IntoIter = $crate::buf::IntoIter<$ty>;

            fn into_iter(self) -> Self::IntoIter {
                $crate::buf::IntoIter::new(self)
            }
        }

        impl<'a> IntoIterator for &'a $ty {
            type Item = &'a u8;
            type IntoIter = ::std::slice::Iter<'a, u8>;

            fn into_iter(self) -> Self::IntoIter {
                self.storage.as_ref().iter()
            }
        }
    };
}

/// Implements `io::Read`, consuming the data from the front.
macro_rules! impl_read {
    ($ty:ty) => {
        impl ::std::io::Read for $ty {
            fn read(&mut self, dst: &mut [u8]) -> ::std::io::Result<usize> {
                let len = ::std::cmp::min(self.len(), dst.len());
                if len > 0 {
                    dst[..len].copy_from_slice(&self[..len]);
                    self.advance_to(len);
                }
                Ok(len)
            }
        }
    };
}

/// Implements `PartialEq` against byte and string slices, arrays, vectors and
/// references, in both directions. Requires `AsRef<[u8]>`.
macro_rules! impl_partial_eq {
    ($ty:ident) => {
        impl PartialEq<[u8]> for $ty {
            fn eq(&self, other: &[u8]) -> bool {
                AsRef::<[u8]>::as_ref(self) == other
            }
        }

        impl<const N: usize> PartialEq<[u8; N]> for $ty {
            fn eq(&self, other: &[u8; N]) -> bool {
                AsRef::<[u8]>::as_ref(self) == other.as_slice()
            }
        }

        impl PartialEq<str> for $ty {
            fn eq(&self, other: &str) -> bool {
                AsRef::<[u8]>::as_ref(self) == other.as_bytes()
            }
        }

        impl PartialEq<Vec<u8>> for $ty {
            fn eq(&self, other: &Vec<u8>) -> bool {
                AsRef::<[u8]>::as_ref(self) == other.as_slice()
            }
        }

        impl PartialEq<String> for $ty {
            fn eq(&self, other: &String) -> bool {
                AsRef::<[u8]>::as_ref(self) == other.as_bytes()
            }
        }

        impl<'a, T: ?Sized> PartialEq<&'a T> for $ty
        where
            $ty: PartialEq<T>,
        {
            fn eq(&self, other: &&'a T) -> bool {
                *self == **other
            }
        }

        impl_partial_eq!(@rev $ty, [u8], str, Vec<u8>, String, &[u8], &str);

        impl<const N: usize> PartialEq<$ty> for [u8; N] {
            fn eq(&self, other: &$ty) -> bool {
                *other == *self
            }
        }

        impl<const N: usize> PartialEq<$ty> for &[u8; N] {
            fn eq(&self, other: &$ty) -> bool {
                *other == *self
            }
        }
    };
    (@rev $ty:ident, $($other:ty),*) => {$(
        impl PartialEq<$ty> for $other {
            fn eq(&self, other: &$ty) -> bool {
                *other == *self
            }
        }
    )*};
}

/// Implements `PartialOrd` against byte and string slices, arrays, vectors
/// and references, in both directions.
macro_rules! impl_partial_ord {
    ($ty:ident) => {
        impl PartialOrd<[u8]> for $ty {
            fn partial_cmp(&self, other: &[u8]) -> Option<::std::cmp::Ordering> {
                self.storage.as_ref().partial_cmp(other)
            }
        }

        impl<const N: usize> PartialOrd<[u8; N]> for $ty {
            fn partial_cmp(&self, other: &[u8; N]) -> Option<::std::cmp::Ordering> {
                self.storage.as_ref().partial_cmp(other.as_slice())
            }
        }

        impl PartialOrd<str> for $ty {
            fn partial_cmp(&self, other: &str) -> Option<::std::cmp::Ordering> {
                self.storage.as_ref().partial_cmp(other.as_bytes())
            }
        }

        impl PartialOrd<Vec<u8>> for $ty {
            fn partial_cmp(&self, other: &Vec<u8>) -> Option<::std::cmp::Ordering> {
                self.storage.as_ref().partial_cmp(other.as_slice())
            }
        }

        impl PartialOrd<String> for $ty {
            fn partial_cmp(&self, other: &String) -> Option<::std::cmp::Ordering> {
                self.storage.as_ref().partial_cmp(other.as_bytes())
            }
        }

        impl<'a, T: ?Sized> PartialOrd<&'a T> for $ty
        where
            $ty: PartialOrd<T>,
        {
            fn partial_cmp(&self, other: &&'a T) -> Option<::std::cmp::Ordering> {
                self.partial_cmp(&**other)
            }
        }

        impl_partial_ord!(@rev $ty, [u8], str, Vec<u8>, String, &[u8], &str);

        impl<const N: usize> PartialOrd<$ty> for [u8; N] {
            fn partial_cmp(&self, other: &$ty) -> Option<::std::cmp::Ordering> {
                other.partial_cmp(self).map(::std::cmp::Ordering::reverse)
            }
        }
    };
    (@rev $ty:ident, $($other:ty),*) => {$(
        impl PartialOrd<$ty> for $other {
            fn partial_cmp(&self, other: &$ty) -> Option<::std::cmp::Ordering> {
                other.partial_cmp(self).map(::std::cmp::Ordering::reverse)
            }
        }
    )*};
}
