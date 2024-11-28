//! Traits and structures to aid consuming and writing HTTP payloads.
use std::{
    error::Error, fmt, marker::PhantomData, mem, pin::Pin, task::Context, task::Poll,
};

use futures_core::Stream;
use ntex_bytes::{Bytes, BytesMut};

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
/// Body size hint
pub enum BodySize {
    None,
    Empty,
    Sized(u64),
    Stream,
}

impl BodySize {
    pub fn is_eof(&self) -> bool {
        matches!(self, BodySize::None | BodySize::Empty | BodySize::Sized(0))
    }
}

/// Interface for types that can be streamed to a peer.
pub trait MessageBody: 'static {
    /// Message body size hind
    fn size(&self) -> BodySize;

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>>;
}

impl MessageBody for () {
    #[inline]
    fn size(&self) -> BodySize {
        BodySize::Empty
    }

    #[inline]
    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        Poll::Ready(None)
    }
}

impl<T: MessageBody> MessageBody for Box<T> {
    #[inline]
    fn size(&self) -> BodySize {
        self.as_ref().size()
    }

    #[inline]
    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        self.as_mut().poll_next_chunk(cx)
    }
}

#[derive(Debug)]
/// Represents http response body
pub enum ResponseBody<B> {
    Body(B),
    Other(Body),
}

impl ResponseBody<Body> {
    pub fn into_body<B>(self) -> ResponseBody<B> {
        match self {
            ResponseBody::Body(b) => ResponseBody::Other(b),
            ResponseBody::Other(b) => ResponseBody::Other(b),
        }
    }
}

impl From<ResponseBody<Body>> for Body {
    fn from(b: ResponseBody<Body>) -> Self {
        match b {
            ResponseBody::Body(b) => b,
            ResponseBody::Other(b) => b,
        }
    }
}

impl<B> From<Body> for ResponseBody<B> {
    fn from(b: Body) -> Self {
        ResponseBody::Other(b)
    }
}

impl<B> ResponseBody<B> {
    #[inline]
    pub fn new(body: B) -> Self {
        ResponseBody::Body(body)
    }

    #[inline]
    pub fn take_body(&mut self) -> ResponseBody<B> {
        std::mem::replace(self, ResponseBody::Other(Body::None))
    }
}

impl<B: MessageBody> ResponseBody<B> {
    pub fn as_ref(&self) -> Option<&B> {
        if let ResponseBody::Body(ref b) = self {
            Some(b)
        } else {
            None
        }
    }
}

impl<B: MessageBody> MessageBody for ResponseBody<B> {
    #[inline]
    fn size(&self) -> BodySize {
        match self {
            ResponseBody::Body(ref body) => body.size(),
            ResponseBody::Other(ref body) => body.size(),
        }
    }

    #[inline]
    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        match self {
            ResponseBody::Body(ref mut body) => body.poll_next_chunk(cx),
            ResponseBody::Other(ref mut body) => body.poll_next_chunk(cx),
        }
    }
}

impl<B: MessageBody + Unpin> Stream for ResponseBody<B> {
    type Item = Result<Bytes, Box<dyn Error>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            ResponseBody::Body(ref mut body) => body.poll_next_chunk(cx),
            ResponseBody::Other(ref mut body) => body.poll_next_chunk(cx),
        }
    }
}

/// Represents various types of http message body.
pub enum Body {
    /// Empty response. `Content-Length` header is not set.
    None,
    /// Zero sized response body. `Content-Length` header is set to `0`.
    Empty,
    /// Specific response body.
    Bytes(Bytes),
    /// Generic message body.
    Message(Box<dyn MessageBody>),
}

impl Body {
    /// Create body from slice (copy)
    pub fn from_slice(s: &[u8]) -> Body {
        Body::Bytes(Bytes::copy_from_slice(s))
    }

    /// Create body from generic message body.
    pub fn from_message<B: MessageBody>(body: B) -> Body {
        Body::Message(Box::new(body))
    }
}

impl MessageBody for Body {
    #[inline]
    fn size(&self) -> BodySize {
        match self {
            Body::None => BodySize::None,
            Body::Empty => BodySize::Empty,
            Body::Bytes(ref bin) => BodySize::Sized(bin.len() as u64),
            Body::Message(ref body) => body.size(),
        }
    }

    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        match self {
            Body::None => Poll::Ready(None),
            Body::Empty => Poll::Ready(None),
            Body::Bytes(ref mut bin) => {
                let len = bin.len();
                if len == 0 {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(mem::take(bin))))
                }
            }
            Body::Message(ref mut body) => body.poll_next_chunk(cx),
        }
    }
}

impl PartialEq for Body {
    fn eq(&self, other: &Body) -> bool {
        match (self, other) {
            (Body::None, Body::None) => true,
            (Body::Empty, Body::Empty) => true,
            (Body::Bytes(ref b), Body::Bytes(ref b2)) => b == b2,
            _ => false,
        }
    }
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Body::None => write!(f, "Body::None"),
            Body::Empty => write!(f, "Body::Empty"),
            Body::Bytes(ref b) => write!(f, "Body::Bytes({:?})", b),
            Body::Message(_) => write!(f, "Body::Message(_)"),
        }
    }
}

impl From<&'static str> for Body {
    fn from(s: &'static str) -> Body {
        Body::Bytes(Bytes::from_static(s.as_ref()))
    }
}

impl From<&'static [u8]> for Body {
    fn from(s: &'static [u8]) -> Body {
        Body::Bytes(Bytes::from_static(s))
    }
}

impl From<Vec<u8>> for Body {
    fn from(vec: Vec<u8>) -> Body {
        Body::Bytes(Bytes::from(vec))
    }
}

impl From<String> for Body {
    fn from(s: String) -> Body {
        s.into_bytes().into()
    }
}

impl<'a> From<&'a String> for Body {
    fn from(s: &'a String) -> Body {
        Body::Bytes(Bytes::copy_from_slice(AsRef::<[u8]>::as_ref(&s)))
    }
}

impl From<Bytes> for Body {
    fn from(s: Bytes) -> Body {
        Body::Bytes(s)
    }
}

impl From<BytesMut> for Body {
    fn from(s: BytesMut) -> Body {
        Body::Bytes(s.freeze())
    }
}

impl<S> From<SizedStream<S>> for Body
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin + 'static,
{
    fn from(s: SizedStream<S>) -> Body {
        Body::from_message(s)
    }
}

impl<S, E> From<BodyStream<S, E>> for Body
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
    E: Error + 'static,
{
    fn from(s: BodyStream<S, E>) -> Body {
        Body::from_message(s)
    }
}

impl<S> From<BoxedBodyStream<S>> for Body
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin + 'static,
{
    fn from(s: BoxedBodyStream<S>) -> Body {
        Body::from_message(s)
    }
}

impl MessageBody for Bytes {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(mem::take(self))))
        }
    }
}

impl MessageBody for BytesMut {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(mem::take(self).freeze())))
        }
    }
}

impl MessageBody for &'static str {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(Bytes::from_static(mem::take(self).as_ref()))))
        }
    }
}

impl MessageBody for &'static [u8] {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(Bytes::from_static(mem::take(self)))))
        }
    }
}

impl MessageBody for Vec<u8> {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(Bytes::from(mem::take(self)))))
        }
    }
}

impl MessageBody for String {
    fn size(&self) -> BodySize {
        BodySize::Sized(self.len() as u64)
    }

    fn poll_next_chunk(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        if self.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(Ok(Bytes::from(mem::take(self).into_bytes()))))
        }
    }
}

/// Type represent streaming body.
///
/// Response does not contain `content-length` header and appropriate transfer encoding is used.
pub struct BodyStream<S, E> {
    stream: S,
    _t: PhantomData<E>,
}

impl<S, E> BodyStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Error,
{
    pub fn new(stream: S) -> Self {
        BodyStream {
            stream,
            _t: PhantomData,
        }
    }
}

impl<S, E> fmt::Debug for BodyStream<S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyStream")
            .field("stream", &std::any::type_name::<S>())
            .field("error", &std::any::type_name::<E>())
            .finish()
    }
}

impl<S, E> MessageBody for BodyStream<S, E>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
    E: Error + 'static,
{
    fn size(&self) -> BodySize {
        BodySize::Stream
    }

    /// Attempts to pull out the next value of the underlying [`Stream`].
    ///
    /// Empty values are skipped to prevent [`BodyStream`]'s transmission being
    /// ended on a zero-length chunk, but rather proceed until the underlying
    /// [`Stream`] ends.
    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        loop {
            return Poll::Ready(match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(ref bytes))) if bytes.is_empty() => continue,
                Poll::Ready(opt) => opt.map(|res| res.map_err(Into::into)),
                Poll::Pending => return Poll::Pending,
            });
        }
    }
}

/// Type represent streaming body.
/// Response does not contain `content-length` header and appropriate transfer encoding is used.
pub struct BoxedBodyStream<S> {
    stream: S,
}

impl<S> BoxedBodyStream<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin,
{
    pub fn new(stream: S) -> Self {
        BoxedBodyStream { stream }
    }
}

impl<S> fmt::Debug for BoxedBodyStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxedBodyStream")
            .field("stream", &std::any::type_name::<S>())
            .finish()
    }
}

impl<S> MessageBody for BoxedBodyStream<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin + 'static,
{
    fn size(&self) -> BodySize {
        BodySize::Stream
    }

    /// Attempts to pull out the next value of the underlying [`Stream`].
    ///
    /// Empty values are skipped to prevent [`BodyStream`]'s transmission being
    /// ended on a zero-length chunk, but rather proceed until the underlying
    /// [`Stream`] ends.
    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        loop {
            return Poll::Ready(match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(ref bytes))) if bytes.is_empty() => continue,
                Poll::Ready(opt) => opt,
                Poll::Pending => return Poll::Pending,
            });
        }
    }
}

/// Type represent streaming body. This body implementation should be used
/// if total size of stream is known. Data get sent as is without using transfer encoding.
pub struct SizedStream<S> {
    size: u64,
    stream: S,
}

impl<S> SizedStream<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin,
{
    pub fn new(size: u64, stream: S) -> Self {
        SizedStream { size, stream }
    }
}

impl<S> fmt::Debug for SizedStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SizedStream")
            .field("size", &self.size)
            .field("stream", &std::any::type_name::<S>())
            .finish()
    }
}

impl<S> MessageBody for SizedStream<S>
where
    S: Stream<Item = Result<Bytes, Box<dyn Error>>> + Unpin + 'static,
{
    fn size(&self) -> BodySize {
        BodySize::Sized(self.size)
    }

    /// Attempts to pull out the next value of the underlying [`Stream`].
    ///
    /// Empty values are skipped to prevent [`SizedStream`]'s transmission being
    /// ended on a zero-length chunk, but rather proceed until the underlying
    /// [`Stream`] ends.
    fn poll_next_chunk(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Box<dyn Error>>>> {
        loop {
            return Poll::Ready(match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(ref bytes))) if bytes.is_empty() => continue,
                Poll::Ready(val) => val,
                Poll::Pending => return Poll::Pending,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, io};

    use futures_util::stream;
    use ntex_util::future::Ready;

    use super::*;

    impl Body {
        pub(crate) fn get_ref(&self) -> &[u8] {
            match *self {
                Body::Bytes(ref bin) => bin,
                _ => panic!(),
            }
        }
    }

    #[ntex::test]
    async fn test_static_str() {
        assert_eq!(Body::from("").size(), BodySize::Sized(0));
        assert_eq!(Body::from("test").size(), BodySize::Sized(4));
        assert_eq!(Body::from("test").get_ref(), b"test");

        assert_eq!("test".size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| "test".poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert_eq!(
            poll_fn(|cx| "test".poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert!(poll_fn(|cx| "".poll_next_chunk(cx)).await.is_none());
    }

    #[ntex::test]
    async fn test_static_bytes() {
        assert_eq!(Body::from(b"test".as_ref()).size(), BodySize::Sized(4));
        assert_eq!(Body::from(b"test".as_ref()).get_ref(), b"test");
        assert_eq!(
            Body::from_slice(b"test".as_ref()).size(),
            BodySize::Sized(4)
        );
        assert_eq!(Body::from_slice(b"test".as_ref()).get_ref(), b"test");

        assert_eq!((&b"test"[..]).size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| (&b"test"[..]).poll_next_chunk(cx))
                .await
                .unwrap()
                .ok(),
            Some(Bytes::from("test"))
        );
        assert_eq!((&b"test"[..]).size(), BodySize::Sized(4));
        assert!(poll_fn(|cx| (&b""[..]).poll_next_chunk(cx)).await.is_none());
    }

    #[ntex::test]
    async fn test_vec() {
        assert_eq!(Body::from(Vec::from("test")).size(), BodySize::Sized(4));
        assert_eq!(Body::from(Vec::from("test")).get_ref(), b"test");

        assert_eq!(Vec::from("test").size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| Vec::from("test").poll_next_chunk(cx))
                .await
                .unwrap()
                .ok(),
            Some(Bytes::from("test"))
        );
        assert_eq!(
            poll_fn(|cx| Vec::from("test").poll_next_chunk(cx))
                .await
                .unwrap()
                .ok(),
            Some(Bytes::from("test"))
        );
        assert!(poll_fn(|cx| Vec::<u8>::new().poll_next_chunk(cx))
            .await
            .is_none());
    }

    #[ntex::test]
    async fn test_bytes() {
        let mut b = Bytes::from("test");
        assert_eq!(Body::from(b.clone()).size(), BodySize::Sized(4));
        assert_eq!(Body::from(b.clone()).get_ref(), b"test");

        assert_eq!(b.size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| b.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert!(poll_fn(|cx| b.poll_next_chunk(cx)).await.is_none(),);
    }

    #[ntex::test]
    async fn test_bytes_mut() {
        let mut b = Body::from(BytesMut::from("test"));
        assert_eq!(b.size(), BodySize::Sized(4));
        assert_eq!(b.get_ref(), b"test");
        assert_eq!(
            poll_fn(|cx| b.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert!(poll_fn(|cx| b.poll_next_chunk(cx)).await.is_none(),);

        let mut b = BytesMut::from("test");
        assert_eq!(b.size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| b.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert_eq!(b.size(), BodySize::Sized(0));
        assert!(poll_fn(|cx| b.poll_next_chunk(cx)).await.is_none(),);
    }

    #[ntex::test]
    async fn test_string() {
        let mut b = "test".to_owned();
        assert_eq!(Body::from(b.clone()).size(), BodySize::Sized(4));
        assert_eq!(Body::from(b.clone()).get_ref(), b"test");
        assert_eq!(Body::from(&b).size(), BodySize::Sized(4));
        assert_eq!(Body::from(&b).get_ref(), b"test");

        assert_eq!(b.size(), BodySize::Sized(4));
        assert_eq!(
            poll_fn(|cx| b.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("test"))
        );
        assert!(poll_fn(|cx| b.poll_next_chunk(cx)).await.is_none(),);
    }

    #[ntex::test]
    async fn test_unit() {
        assert_eq!(().size(), BodySize::Empty);
        assert!(poll_fn(|cx| ().poll_next_chunk(cx)).await.is_none());
    }

    #[ntex::test]
    async fn test_box() {
        let mut val = Box::new(());
        assert_eq!(val.size(), BodySize::Empty);
        assert!(poll_fn(|cx| val.poll_next_chunk(cx)).await.is_none());
    }

    #[ntex::test]
    #[allow(clippy::eq_op)]
    async fn test_body_eq() {
        assert!(Body::None == Body::None);
        assert!(Body::None != Body::Empty);
        assert!(Body::Empty == Body::Empty);
        assert!(Body::Empty != Body::None);
        assert!(
            Body::Bytes(Bytes::from_static(b"1")) == Body::Bytes(Bytes::from_static(b"1"))
        );
        assert!(Body::Bytes(Bytes::from_static(b"1")) != Body::None);
    }

    #[ntex::test]
    async fn test_body_debug() {
        assert!(format!("{:?}", Body::None).contains("Body::None"));
        assert!(format!("{:?}", Body::Empty).contains("Body::Empty"));
        assert!(format!("{:?}", Body::Bytes(Bytes::from_static(b"1"))).contains('1'));
    }

    #[ntex::test]
    async fn body_stream() {
        let st = BodyStream::new(stream::once(Ready::<_, io::Error>::Ok(Bytes::from("1"))));
        assert!(format!("{:?}", st).contains("BodyStream"));
        let body: Body = st.into();
        assert!(format!("{:?}", body).contains("Body::Message(_)"));
        assert!(body != Body::None);

        let res = ResponseBody::new(body);
        assert!(res.as_ref().is_some());
    }

    #[ntex::test]
    async fn boxed_body_stream() {
        let st = BoxedBodyStream::new(stream::once(Ready::<_, Box<dyn Error>>::Ok(
            Bytes::from("1"),
        )));
        assert!(format!("{:?}", st).contains("BoxedBodyStream"));
        let body: Body = st.into();
        assert!(format!("{:?}", body).contains("Body::Message(_)"));
        assert!(body != Body::None);

        let res = ResponseBody::new(body);
        assert!(res.as_ref().is_some());
    }

    #[ntex::test]
    async fn body_skips_empty_chunks() {
        let mut body = BodyStream::new(stream::iter(
            ["1", "", "2"]
                .iter()
                .map(|&v| Ok(Bytes::from(v)) as Result<Bytes, io::Error>),
        ));
        assert_eq!(
            poll_fn(|cx| body.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("1")),
        );
        assert_eq!(
            poll_fn(|cx| body.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("2")),
        );
    }

    #[ntex::test]
    async fn sized_skips_empty_chunks() {
        let mut body = SizedStream::new(
            2,
            stream::iter(["1", "", "2"].iter().map(|&v| Ok(Bytes::from(v)))),
        );
        assert!(format!("{:?}", body).contains("SizedStream"));
        assert_eq!(
            poll_fn(|cx| body.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("1")),
        );
        assert_eq!(
            poll_fn(|cx| body.poll_next_chunk(cx)).await.unwrap().ok(),
            Some(Bytes::from("2")),
        );
    }
}
