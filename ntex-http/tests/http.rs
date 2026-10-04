use std::{error::Error as StdError, future::poll_fn, io, pin::Pin, rc::Rc};

use futures_util::{StreamExt, stream};
use ntex_bytes::{ByteString, Bytes, BytesMut};
use ntex_http::body::{
    Body, BodySize, BodyStream, BoxedBodyStream, MessageBody, ResponseBody, SizedStream,
};
use ntex_http::error::{InvalidHeaderName, InvalidMethod, InvalidStatusCode, InvalidUri};
use ntex_http::header::{
    self, ContentEncoding, HeaderName, HeaderValue, InvalidHeaderValue, ToStrError,
};
use ntex_http::{Error, HeaderMap, Value};

async fn next<B: MessageBody>(b: &mut B) -> Option<Result<Bytes, Rc<dyn StdError>>> {
    poll_fn(|cx| b.poll_next_chunk(cx)).await
}

#[test]
fn body_size() {
    assert!(BodySize::None.is_eof());
    assert!(BodySize::Empty.is_eof());
    assert!(BodySize::Sized(0).is_eof());
    assert!(!BodySize::Sized(1).is_eof());
    assert!(!BodySize::Stream.is_eof());
}

#[ntex::test]
async fn response_body() {
    let mut b = ResponseBody::new(Bytes::from_static(b"abc"));
    assert_eq!(b.as_ref(), Some(&Bytes::from_static(b"abc")));
    assert_eq!(b.size(), BodySize::Sized(3));
    assert!(format!("{b:?}").starts_with("Body("));

    let mut taken = b.take_body();
    assert_eq!(b.as_ref(), None);
    assert_eq!(b.size(), BodySize::None);
    assert!(next(&mut b).await.is_none());
    assert_eq!(next(&mut taken).await.unwrap().unwrap(), "abc");
    assert!(next(&mut taken).await.is_none());

    // Stream impl for both variants
    let mut b = ResponseBody::new(Bytes::from_static(b"s1"));
    assert_eq!(b.next().await.unwrap().unwrap(), "s1");
    assert!(b.next().await.is_none());
    let mut b: ResponseBody<Bytes> = Body::from("s2").into();
    assert_eq!(Pin::new(&mut b).next().await.unwrap().unwrap(), "s2");
    assert!(b.next().await.is_none());

    // into_body / From conversions
    let b: ResponseBody<Body> = ResponseBody::new(Body::from("x"));
    let b: ResponseBody<Bytes> = b.into_body();
    assert!(matches!(b, ResponseBody::Other(Body::Bytes(_))));
    let b: ResponseBody<Body> = ResponseBody::Other(Body::Empty);
    let b: ResponseBody<()> = b.into_body();
    assert_eq!(b.size(), BodySize::Empty);

    assert_eq!(
        Body::from(ResponseBody::new(Body::from("a"))),
        Body::from("a")
    );
    assert_eq!(
        Body::from(ResponseBody::<Body>::Other(Body::Empty)),
        Body::Empty
    );
}

#[ntex::test]
async fn body_variants() {
    assert_eq!(Body::from_slice(b"abc"), Body::from("abc"));
    assert_eq!(Body::from(&b"abc"[..]), Body::from("abc"));
    assert_eq!(Body::from(String::from("abc")), Body::from("abc"));
    assert_eq!(Body::from(&String::from("abc")), Body::from("abc"));
    assert_eq!(Body::from(BytesMut::from(&b"abc"[..])), Body::from("abc"));
    assert_ne!(Body::None, Body::Empty);
    assert_ne!(Body::from_message(()), Body::from_message(()));

    let mut b = Body::from(Bytes::new());
    assert_eq!(b.size(), BodySize::Sized(0));
    assert!(next(&mut b).await.is_none());

    let mut b = Body::from_message(Bytes::from_static(b"m"));
    assert_eq!(b.size(), BodySize::Sized(1));
    assert_eq!(format!("{b:?}"), "Body::Message(_)");
    assert_eq!(next(&mut b).await.unwrap().unwrap(), "m");
    assert!(next(&mut b).await.is_none());

    let mut b = Body::None;
    assert!(next(&mut b).await.is_none());
    let mut b = Body::Empty;
    assert!(next(&mut b).await.is_none());
}

#[ntex::test]
async fn streams() {
    // BodyStream maps errors
    let mut b = Body::from(BodyStream::new(stream::iter(vec![
        Ok(Bytes::from_static(b"a")),
        Err(io::Error::other("boom")),
    ])));
    assert_eq!(b.size(), BodySize::Stream);
    assert_eq!(next(&mut b).await.unwrap().unwrap(), "a");
    assert_eq!(next(&mut b).await.unwrap().unwrap_err().to_string(), "boom");
    assert!(next(&mut b).await.is_none());

    let s = BodyStream::new(stream::empty::<Result<Bytes, io::Error>>());
    assert!(format!("{s:?}").starts_with("BodyStream { stream: "));

    let mut b = BodyStream::new(stream::pending::<Result<Bytes, io::Error>>());
    assert!(poll_fn(|cx| std::task::Poll::Ready(b.poll_next_chunk(cx).is_pending())).await);

    // BoxedBodyStream
    let err: Rc<dyn StdError> = Rc::new(io::Error::other("e"));
    let mut b = Body::from(BoxedBodyStream::new(stream::iter(vec![
        Ok(Bytes::from_static(b"a")),
        Err(err),
    ])));
    assert_eq!(b.size(), BodySize::Stream);
    assert_eq!(next(&mut b).await.unwrap().unwrap(), "a");
    assert!(next(&mut b).await.unwrap().is_err());
    assert!(next(&mut b).await.is_none());

    let s = BoxedBodyStream::new(stream::empty());
    assert!(format!("{s:?}").starts_with("BoxedBodyStream { stream: "));
    let mut b = BoxedBodyStream::new(stream::pending());
    assert!(poll_fn(|cx| std::task::Poll::Ready(b.poll_next_chunk(cx).is_pending())).await);

    // SizedStream
    let mut b = Body::from(SizedStream::new(
        2,
        stream::iter(vec![Ok(Bytes::from_static(b"ab"))]),
    ));
    assert_eq!(b.size(), BodySize::Sized(2));
    assert_eq!(next(&mut b).await.unwrap().unwrap(), "ab");
    assert!(next(&mut b).await.is_none());

    let s = SizedStream::new(5, stream::empty());
    assert!(format!("{s:?}").starts_with("SizedStream { size: 5, stream: "));
    let mut b = SizedStream::new(1, stream::pending());
    assert!(poll_fn(|cx| std::task::Poll::Ready(b.poll_next_chunk(cx).is_pending())).await);
}

#[test]
fn errors() {
    let e: Error = http::StatusCode::from_u16(1).unwrap_err().into();
    assert!(e.is::<InvalidStatusCode>());
    assert_eq!(e.to_string(), "Invalid status code");

    let e: Error = http::Method::from_bytes(b"").unwrap_err().into();
    assert!(e.is::<InvalidMethod>());
    assert_eq!(e.to_string(), "Invalid HTTP method");

    let e: Error = "\0".parse::<http::Uri>().unwrap_err().into();
    assert!(e.is::<InvalidUri>());
    assert_eq!(e.to_string(), "Invalid URI");

    let mut parts = http::uri::Parts::default();
    parts.scheme = Some(http::uri::Scheme::HTTP);
    let parts_err = http::Uri::from_parts(parts).unwrap_err();
    let e: Error = parts_err.into();
    assert!(e.is::<InvalidUri>());

    let e: Error = HeaderName::from_bytes(b" ").unwrap_err().into();
    assert!(e.is::<InvalidHeaderName>());
    assert_eq!(e.to_string(), "Invalid HTTP header name");

    let e: Error = HeaderValue::from_str("\n").unwrap_err().into();
    assert!(e.is::<InvalidHeaderValue>());
    assert!(format!("{e:?}").starts_with("ntex_http::Error("));

    // http::Error conversions
    let conv = |e: http::Error| -> Error { e.into() };
    assert!(conv(http::StatusCode::from_u16(1).unwrap_err().into()).is::<InvalidStatusCode>());
    assert!(conv(http::Method::from_bytes(b"").unwrap_err().into()).is::<InvalidMethod>());
    assert!(conv("\0".parse::<http::Uri>().unwrap_err().into()).is::<InvalidUri>());
    assert!(conv(http::HeaderName::from_bytes(b" ").unwrap_err().into()).is::<InvalidHeaderName>());
    assert!(
        conv(http::HeaderValue::from_bytes(b"\n").unwrap_err().into()).is::<InvalidHeaderValue>()
    );

    // InvalidUriParts wrapped in http::Error
    let mut parts = http::uri::Parts::default();
    parts.scheme = Some(http::uri::Scheme::HTTP);
    let e = conv(http::Uri::from_parts(parts).unwrap_err().into());
    assert!(e.is::<InvalidUri>(), "{e}");
    assert!(e.source().is_none());

    // direct conversions to crate error types
    let _: InvalidStatusCode = http::StatusCode::from_u16(1).unwrap_err().into();
    let _: InvalidMethod = http::Method::from_bytes(b"").unwrap_err().into();
    let _: InvalidUri = "\0".parse::<http::Uri>().unwrap_err().into();
    let _: InvalidHeaderName = http::HeaderName::from_bytes(b" ").unwrap_err().into();
    let _: InvalidHeaderValue = http::HeaderValue::from_bytes(b"\n").unwrap_err().into();
}

#[test]
fn content_encoding() {
    for (s, enc, name, q) in [
        ("br", ContentEncoding::Br, "br", 1.1),
        (" GZIP ", ContentEncoding::Gzip, "gzip", 1.0),
        ("Deflate", ContentEncoding::Deflate, "deflate", 0.9),
        ("identity", ContentEncoding::Identity, "identity", 0.1),
        ("unknown", ContentEncoding::Identity, "identity", 0.1),
    ] {
        let e = ContentEncoding::from(s);
        assert_eq!(e, enc);
        assert_eq!(e.as_str(), name);
        assert!((e.quality() - q).abs() < f64::EPSILON);
    }
    assert_eq!(ContentEncoding::Auto.as_str(), "identity");
    assert!((ContentEncoding::Auto.quality() - 0.1).abs() < f64::EPSILON);
    assert!(ContentEncoding::Gzip.is_compressed());
    assert!(ContentEncoding::Deflate.is_compressed());
}

#[test]
fn header_map() {
    let mut map = HeaderMap::with_capacity(4);
    assert!(map.capacity() >= 4);
    map.reserve(10);
    assert!(map.capacity() >= 10);
    assert!(map.is_empty());

    map.append(header::ACCEPT, HeaderValue::from_static("a"));
    map.append(header::ACCEPT, HeaderValue::from_static("b"));
    map.append(header::ACCEPT, HeaderValue::from_static("c"));
    map.insert(header::HOST, HeaderValue::from_static("h"));
    assert_eq!(map.len(), 2);

    // AsName impls
    let name = String::from("accept");
    assert_eq!(map.get(&name).unwrap(), "a");
    assert_eq!(map.get(name.clone()).unwrap(), "a");
    assert_eq!(map.get(&header::ACCEPT).unwrap(), "a");
    assert!(map.get("bad name").is_none());
    assert!(map.get_all("bad name").next().is_none());
    assert!(!map.contains_key("bad name"));
    assert!(map.contains_key("host"));
    assert!(map.contains_key(&header::HOST));

    *map.get_mut("accept").unwrap() = HeaderValue::from_static("a2");
    assert_eq!(map.get(header::ACCEPT).unwrap(), "a2");
    *map.get_mut(header::HOST).unwrap() = HeaderValue::from_static("h2");
    assert_eq!(map.get("host").unwrap(), "h2");
    assert!(map.get_mut("bad name").is_none());
    assert!(map.get_mut("missing").is_none());

    let all: Vec<_> = map.get_all("accept").collect();
    assert_eq!(all, ["a2", "b", "c"]);
    let mut all = map.get_all("host");
    assert_eq!(all.next().unwrap(), "h2");
    assert!(all.next().is_none());
    assert!(all.next().is_none());

    let mut keys: Vec<_> = map.keys().map(HeaderName::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["accept", "host"]);

    let mut items: Vec<_> = (&map)
        .into_iter()
        .map(|(k, v)| format!("{k}={}", v.to_str().unwrap()))
        .collect();
    items.sort();
    assert_eq!(items, ["accept=a2", "accept=b", "accept=c", "host=h2"]);
    assert_eq!(map.iter_inner().count(), 2);

    let dbg = format!("{map:?}");
    assert!(dbg.contains("\"accept\": \"b\""), "{dbg}");
    assert!(dbg.contains("\"host\": \"h2\""), "{dbg}");

    map.remove("bad name");
    map.remove("accept");
    assert!(!map.contains_key("accept"));
    map.remove(header::HOST);
    assert!(map.is_empty());

    map.insert(header::HOST, HeaderValue::from_static("h"));
    map.clear();
    assert!(map.is_empty());
    assert_eq!(HeaderMap::default(), HeaderMap::new());
}

#[test]
fn header_map_value() {
    let mut v = Value::from(HeaderValue::from_static("a"));
    v.extend([HeaderValue::from_static("b"), HeaderValue::from_static("c")]);
    assert_eq!(v.clone().into_iter().size_hint(), (3, Some(3)));
    assert_eq!(v.into_iter().collect::<Vec<_>>(), ["a", "b", "c"]);

    let v = Value::from(&HeaderValue::from_static("one"));
    let mut it = v.into_iter();
    assert_eq!(it.size_hint(), (1, Some(1)));
    assert_eq!(it.next().unwrap(), "one");
    assert!(it.next().is_none());
}

#[test]
#[allow(clippy::op_ref, clippy::cmp_owned)]
fn header_value() {
    let v = unsafe { HeaderValue::from_shared_unchecked(Bytes::from_static(b"raw")) };
    assert_eq!(v, "raw");

    assert!(HeaderValue::from_str("a\nb").is_err());
    assert!(HeaderValue::from_bytes(b"a\x7f").is_err());
    assert!(HeaderValue::from_shared(Bytes::from_static(b"\0")).is_err());

    let v = HeaderValue::from_bytes(b"caf\xc3\xa9").unwrap();
    assert_eq!(v.to_str().unwrap(), "caf\u{e9}");
    assert_eq!(format!("{v:?}"), "\"caf\\xc3\\xa9\"");

    let v = HeaderValue::from_bytes(b"caf\xe9").unwrap();
    let err: ToStrError = v.to_str().unwrap_err();
    assert!(!err.to_string().is_empty());
    let v = unsafe { HeaderValue::from_shared_unchecked(Bytes::from_static(b"a\x00\x7fb")) };
    assert_eq!(v.to_str().unwrap(), "a\x00\x7fb");

    assert_eq!(HeaderValue::try_from("s").unwrap(), "s");
    assert_eq!(HeaderValue::try_from(&String::from("s")).unwrap(), "s");
    assert_eq!(HeaderValue::try_from(&ByteString::from("s")).unwrap(), "s");
    assert_eq!(HeaderValue::try_from(&b"s"[..]).unwrap(), "s");
    assert!(HeaderValue::try_from(&String::from("\n")).is_err());

    let v = HeaderValue::from_static("x");
    assert!(&v == v);
    assert!(&v <= v);
    assert_eq!(String::from("x"), v);
    assert!(String::from("w") < v);

    // http interop preserves the sensitive flag
    let mut v = HeaderValue::from_static("secret");
    v.set_sensitive(true);
    let hv: http::HeaderValue = (&v).into();
    assert!(hv.is_sensitive());
    let back = HeaderValue::from(&hv);
    assert!(back.is_sensitive());
    let hv: http::HeaderValue = v.into();
    let back = HeaderValue::from(hv);
    assert!(back.is_sensitive());
    assert_eq!(format!("{back:?}"), "Sensitive");
}
