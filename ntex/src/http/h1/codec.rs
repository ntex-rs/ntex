use std::{cell::Cell, fmt};

use bitflags::bitflags;

use crate::codec::{Decoder, Encoder};
use crate::http::body::BodySize;
use crate::http::config::{DateService, HttpServiceConfig};
use crate::http::error::{DecodeError, EncodeError};
use crate::http::message::ConnectionType;
use crate::http::{Method, Version, request::Request, response::Response};
use crate::{Cfg, util::BytePages, util::BytesMut};

use super::{Message, decoder, decoder::PayloadType, encoder};

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const HEAD              = 0b0000_0001;
        const STREAM            = 0b0000_0010;
        const KEEPALIVE_ENABLED = 0b0000_0100;
        const UPGRADE           = 0b0000_1000;
    }
}

/// Stateful HTTP/1 request decoder and response encoder.
///
/// The codec tracks the version, connection behavior, request method, and
/// streaming state of the most recently decoded request.
///
/// # Decoding
///
/// [`Decoder::decode`] incrementally consumes one request head and returns its
/// [`Request`] together with a [`PayloadType`]. For
/// [`PayloadType::Payload`], pass subsequent bytes to the returned payload
/// decoder until framing completes before decoding another request head. Bytes
/// for a pipelined request can already remain in the input buffer.
/// [`PayloadType::Stream`] ends HTTP message framing; transfer the connection
/// and any buffered bytes to the upgraded protocol instead of decoding another
/// HTTP request.
///
/// `Ok(None)` means that more bytes are required. A [`DecodeError`] indicates
/// invalid framing or a configured request-head limit and should be treated as
/// a connection-level protocol failure.
///
/// # Encoding
///
/// [`Encoder::encodev`] accepts a
/// [`Message<(Response<()>, BodySize)>`](Message). Encode the response head
/// first, followed by body chunks and a final `Message::Chunk(None)` when the
/// response has a body. The codec selects fixed-length, chunked, or
/// connection-close framing from the response, request method, version, and
/// supplied [`BodySize`]. An [`EncodeError`] indicates invalid response
/// encoding or an incomplete fixed-length body.
///
/// The codec only transforms buffers; it does not perform I/O, flush output,
/// or apply transport backpressure.
pub struct Codec {
    con_id: usize,
    decoder: decoder::MessageDecoder<Request>,
    version: Cell<Version>,
    ctype: Cell<ConnectionType>,
    pub(super) cfg: Cfg<HttpServiceConfig>,

    // encoder part
    flags: Cell<Flags>,
    encoder: encoder::MessageEncoder<Response<()>>,
}

impl Clone for Codec {
    fn clone(&self) -> Self {
        Codec {
            con_id: self.con_id,
            decoder: self.decoder.clone(),
            version: self.version.clone(),
            cfg: self.cfg.clone(),
            ctype: self.ctype.clone(),
            flags: self.flags.clone(),
            encoder: self.encoder.clone(),
        }
    }
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("h1::Codec")
            .field("con_id", &self.con_id)
            .field("version", &self.version)
            .field("flags", &self.flags)
            .field("ctype", &self.ctype)
            .field("encoder", &self.encoder)
            .field("decoder", &self.decoder)
            .finish()
    }
}

impl Codec {
    /// Creates an HTTP/1 codec.
    ///
    /// `con_id` identifies the connection in decoded request heads. Protocol
    /// limits and keep-alive behavior are read from `cfg`.
    pub fn new(con_id: usize, cfg: Cfg<HttpServiceConfig>) -> Self {
        let flags = if cfg.ka_enabled {
            Flags::KEEPALIVE_ENABLED
        } else {
            Flags::empty()
        };
        let ctype = if cfg.ka_enabled {
            ConnectionType::KeepAlive
        } else {
            ConnectionType::Close
        };
        let decoder = decoder::MessageDecoder::new(cfg.clone());

        Codec {
            cfg,
            con_id,
            decoder,
            flags: Cell::new(flags),
            version: Cell::new(Version::HTTP_11),
            ctype: Cell::new(ctype),
            encoder: encoder::MessageEncoder::default(),
        }
    }

    pub(super) fn is_reading_hdrs(&self) -> bool {
        self.decoder.is_reading_hdrs()
    }

    #[inline]
    /// Returns whether the most recently decoded request upgrades the
    /// connection.
    ///
    /// The flag is updated each time a request is decoded. It is not cleared
    /// when the dispatcher hands the connection to an upgrade handler, so the
    /// handler's codec still reports the upgrade.
    pub fn upgrade(&self) -> bool {
        self.flags.get().contains(Flags::UPGRADE)
    }

    #[inline]
    /// Returns whether the current HTTP connection state is persistent.
    ///
    /// Before the first request is decoded, this reflects whether keep-alive
    /// is enabled in the service configuration. Decoding a request or encoding
    /// a response can update the value.
    pub fn keepalive(&self) -> bool {
        self.ctype.get() == ConnectionType::KeepAlive
    }

    #[inline]
    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        DateService.set_date_header(dst);
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    pub(super) fn reset_upgrade(&self) {
        let mut flags = self.flags.get();
        flags.remove(Flags::STREAM);
        self.flags.set(flags);
        self.ctype.set(ConnectionType::Close);
    }
}

impl Decoder for Codec {
    type Item = (Request, PayloadType);
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some((mut req, payload)) = self.decoder.decode(src)? {
            let head = req.head_mut();
            head.id = self.con_id;
            let mut flags = self.flags.get();
            flags.set(Flags::HEAD, head.method == Method::HEAD);
            self.flags.set(flags);
            self.version.set(head.version);

            let ctype = head.connection_type();
            flags.set(Flags::UPGRADE, ctype == ConnectionType::Upgrade);
            self.flags.set(flags);
            if ctype == ConnectionType::KeepAlive && !flags.contains(Flags::KEEPALIVE_ENABLED) {
                self.ctype.set(ConnectionType::Close);
            } else {
                self.ctype.set(ctype);
            }

            if let PayloadType::Stream(_) = payload {
                self.insert_flags(Flags::STREAM);
            }
            Ok(Some((req, payload)))
        } else {
            Ok(None)
        }
    }
}

impl Encoder for Codec {
    type Item = Message<(Response<()>, BodySize)>;
    type Error = EncodeError;

    fn encodev(&self, item: Self::Item, dst: &mut BytePages) -> Result<(), Self::Error> {
        match item {
            Message::Item((mut res, length)) => {
                // set response version
                res.head_mut().version = self.version.get();

                // connection status
                if let Some(ct) = res.head().ctype()
                    && ct != ConnectionType::KeepAlive
                {
                    self.ctype.set(ct);
                }

                // encode message
                let ctype = self.encoder.encode(
                    dst,
                    &res,
                    self.flags.get().contains(Flags::HEAD),
                    self.flags.get().contains(Flags::STREAM),
                    self.version.get(),
                    length,
                    self.ctype.get(),
                    None,
                )?;
                self.ctype.set(ctype);
            }
            Message::Chunk(Some(bytes)) => {
                self.encoder.encode_chunk(bytes, dst)?;
            }
            Message::Chunk(None) => {
                self.encoder.encode_eof(dst)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SharedCfg,
        http::{HttpMessage, KeepAlive, h1::PayloadItem},
        util::Bytes,
    };

    /// Bodyless statuses do not write body bytes or length headers.
    #[crate::rt_test]
    async fn test_bodyless_status_has_no_body() {
        use crate::http::StatusCode;

        let cfg: SharedCfg = SharedCfg::new("DBG").add(HttpServiceConfig::new()).into();
        for status in [
            StatusCode::CONTINUE,
            StatusCode::from_u16(103).unwrap(),
            StatusCode::NO_CONTENT,
            StatusCode::NOT_MODIFIED,
        ] {
            for size in [BodySize::Sized(3), BodySize::Stream] {
                let codec = Codec::new(0, cfg.get());
                let mut buf = BytesMut::from("GET / HTTP/1.1\r\n\r\n");
                codec.decode(&mut buf).unwrap().unwrap();

                let mut out = BytePages::default();
                let res = Response::with_body(status, ());
                codec.encodev(Message::Item((res, size)), &mut out).unwrap();
                codec
                    .encodev(Message::Chunk(Some(Bytes::from_static(b"abc"))), &mut out)
                    .unwrap();
                codec.encodev(Message::Chunk(None), &mut out).unwrap();

                let mut data = Vec::new();
                while let Some(chunk) = out.take() {
                    data.extend_from_slice(&chunk);
                }
                let data = String::from_utf8(data).unwrap();
                assert!(data.ends_with("\r\n\r\n"), "{status} {size:?}: {data:?}");
                assert!(
                    !data.contains("content-length"),
                    "{status} {size:?}: {data:?}"
                );
                assert!(
                    !data.contains("transfer-encoding"),
                    "{status} {size:?}: {data:?}"
                );
            }
        }
    }

    fn encode_stream(req: &str, res: Response<()>) -> (String, bool) {
        let cfg: SharedCfg = SharedCfg::new("DBG").add(HttpServiceConfig::new()).into();
        let codec = Codec::new(0, cfg.get());
        let mut buf = BytesMut::from(req);
        codec.decode(&mut buf).unwrap().unwrap();

        let mut out = BytePages::default();
        codec
            .encodev(Message::Item((res, BodySize::Stream)), &mut out)
            .unwrap();
        codec
            .encodev(Message::Chunk(Some(Bytes::from_static(b"abc"))), &mut out)
            .unwrap();
        codec.encodev(Message::Chunk(None), &mut out).unwrap();

        let mut data = Vec::new();
        while let Some(chunk) = out.take() {
            data.extend_from_slice(&chunk);
        }
        (String::from_utf8(data).unwrap(), codec.keepalive())
    }

    /// HTTP/1.0 streaming responses are not chunked, a streaming response
    /// delimited by connection close closes the connection.
    #[crate::rt_test]
    async fn test_http10_stream_response_is_not_chunked() {
        use crate::http::StatusCode;

        let (data, keepalive) = encode_stream(
            "GET / HTTP/1.0\r\nconnection: keep-alive\r\n\r\n",
            Response::with_body(StatusCode::OK, ()),
        );
        assert!(data.starts_with("HTTP/1.0 200 OK\r\n"), "{data:?}");
        assert!(!data.contains("transfer-encoding"), "{data:?}");
        assert!(!data.contains("keep-alive"), "{data:?}");
        assert!(data.ends_with("\r\n\r\nabc"), "{data:?}");
        assert!(!keepalive);

        let (data, keepalive) = encode_stream(
            "GET / HTTP/1.1\r\n\r\n",
            Response::with_body(StatusCode::OK, ()),
        );
        assert!(data.contains("transfer-encoding: chunked\r\n"), "{data:?}");
        assert!(data.ends_with("3\r\nabc\r\n0\r\n\r\n"), "{data:?}");
        assert!(keepalive);

        let mut res = Response::with_body(StatusCode::OK, ());
        res.head_mut().no_chunking(true);
        let (data, keepalive) = encode_stream("GET / HTTP/1.1\r\n\r\n", res);
        assert!(data.contains("connection: close\r\n"), "{data:?}");
        assert!(data.ends_with("\r\n\r\nabc"), "{data:?}");
        assert!(!keepalive);
    }

    #[test]
    fn test_http_request_chunked_payload_and_next_message() {
        let cfg: SharedCfg = SharedCfg::new("DBG").add(HttpServiceConfig::new()).into();

        let codec = Codec::new(0, cfg.get());
        assert!(format!("{codec:?}").contains("h1::Codec"));

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let (req, pl) = codec.decode(&mut buf).unwrap().unwrap();
        let PayloadType::Payload(pl) = pl else { panic!() };

        assert_eq!(req.method(), Method::GET);
        assert!(req.chunked().unwrap());

        buf.extend(
            b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n\
               POST /test2 HTTP/1.1\r\n\
               transfer-encoding: chunked\r\n\r\n"
                .iter(),
        );

        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg, PayloadItem::Chunk(Bytes::from_static(b"data")));

        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg, PayloadItem::Chunk(Bytes::from_static(b"line")));

        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg, PayloadItem::Eof);

        // decode next message
        let (req, _pl) = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(*req.method(), Method::POST);
        assert!(req.chunked().unwrap());

        let codec = Codec::new(0, cfg.get());
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: upgrade\r\n\r\n",
        );
        let _item = codec.decode(&mut buf).unwrap().unwrap();
        assert!(codec.upgrade());
        assert!(!codec.keepalive());
        codec.reset_upgrade();
        assert!(codec.upgrade());
        assert!(!codec.keepalive());

        let cfg: SharedCfg = SharedCfg::new("DBG")
            .add(HttpServiceConfig::new().set_keepalive(KeepAlive::Disabled))
            .into();
        let codec = Codec::new(0, cfg.get());
        assert!(!codec.upgrade());
        assert!(!codec.keepalive());
    }
}
