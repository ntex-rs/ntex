pub mod buffer;
pub mod counter;
mod extensions;
pub mod inflight;
pub mod keepalive;
pub mod sink;
pub mod stream;
pub mod time;
pub mod timeout;
pub mod variant;

pub use self::extensions::Extensions;

pub use ntex_bytes::{Buf, BufMut, ByteString, Bytes, BytesMut};
pub use ntex_util::future::*;

pub type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;
pub type HashSet<V> = std::collections::HashSet<V, ahash::RandomState>;
