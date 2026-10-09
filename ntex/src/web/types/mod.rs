//! Extractor types

pub(in crate::web) mod form;
pub(in crate::web) mod json;
mod path;
pub(in crate::web) mod payload;
mod query;

pub use self::form::{Form, FormConfig};
pub use self::json::{Json, JsonConfig};
pub use self::path::Path;
pub use self::payload::{Payload, PayloadConfig};
pub use self::query::Query;

use crate::http::{error::PayloadError, helpers::take_trimmed};
use crate::util::{Bytes, BytesMut, Stream, stream_recv};

/// Reads the complete body, failing with `overflow(size)` once `size > limit`.
///
/// A single-chunk body is copied only if it is a part of a larger buffer,
/// which it would otherwise keep alive.
async fn read_body<S, E>(
    stream: &mut S,
    limit: usize,
    length: Option<usize>,
    overflow: impl FnOnce(usize) -> E,
) -> Result<Bytes, E>
where
    S: Stream<Item = Result<Bytes, PayloadError>> + Unpin,
    E: From<PayloadError>,
{
    let mut first = match stream_recv(stream).await {
        Some(item) => item?,
        None => return Ok(Bytes::new()),
    };
    if first.len() > limit {
        return Err(overflow(first.len()));
    }
    let Some(chunk) = stream_recv(stream).await else {
        first.trimdown();
        return Ok(first);
    };
    let chunk = chunk?;
    let size = first.len() + chunk.len();
    if size > limit {
        return Err(overflow(size));
    }

    let mut body = BytesMut::with_capacity(length.unwrap_or(0).clamp(size, limit));
    body.extend_from_slice(&first);
    body.extend_from_slice(&chunk);
    while let Some(item) = stream_recv(stream).await {
        let chunk = item?;
        let size = body.len() + chunk.len();
        if size > limit {
            return Err(overflow(size));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(take_trimmed(&mut body))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, pin::Pin, task::Context, task::Poll};

    use super::*;

    struct Chunks(VecDeque<Result<Bytes, PayloadError>>);

    impl Stream for Chunks {
        type Item = Result<Bytes, PayloadError>;

        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    fn chunks(items: &[&'static [u8]]) -> Chunks {
        Chunks(items.iter().map(|b| Ok(Bytes::from_static(b))).collect())
    }

    #[crate::rt_test]
    async fn test_read_body() {
        let data: &'static [u8] = b"single chunk";
        let body = read_body(&mut chunks(&[data]), 64, None, |_| PayloadError::Overflow)
            .await
            .unwrap();
        assert_eq!(
            body.as_ptr(),
            data.as_ptr(),
            "single chunk must not be copied"
        );

        let body = read_body(&mut chunks(&[]), 64, None, |_| PayloadError::Overflow)
            .await
            .unwrap();
        assert!(body.is_empty());

        let body = read_body(&mut chunks(&[b"ab", b"cd", b"ef"]), 6, Some(6), |_| {
            PayloadError::Overflow
        })
        .await
        .unwrap();
        assert_eq!(body, Bytes::from_static(b"abcdef"));

        // the body does not keep the larger buffer it is a part of
        let page = Bytes::from(vec![b'x'; 32 * 1024]);
        let mut stream = Chunks([Ok(page.slice(..1024))].into());
        let body = read_body(&mut stream, 64 * 1024, None, |_| PayloadError::Overflow)
            .await
            .unwrap();
        assert_eq!(body, page.slice(..1024));
        assert_ne!(body.as_ptr(), page.as_ptr());

        // nor the unused part of the buffer it is read into
        let chunk = Bytes::from(vec![b'x'; 1000]);
        let mut stream = Chunks(std::iter::repeat_n(Ok(chunk), 33).collect());
        let body = read_body(&mut stream, 64 * 1024, None, |_| PayloadError::Overflow)
            .await
            .unwrap();
        assert_eq!(body.len(), 33 * 1000);
        let mut copy = body.clone();
        copy.trimdown();
        assert_eq!(copy.as_ptr(), body.as_ptr(), "the body has no unused space");

        for items in [
            &[&b"abcdefg"[..]][..],
            &[b"abcd", b"efg"],
            &[b"ab", b"cd", b"efg"],
        ] {
            let mut size = 0;
            let res = read_body(&mut chunks(items), 6, None, |s| {
                size = s;
                PayloadError::Overflow
            })
            .await;
            assert!(matches!(res, Err(PayloadError::Overflow)));
            assert_eq!(size, 7);
        }
    }
}
