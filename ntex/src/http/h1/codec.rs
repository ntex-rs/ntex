use std::{cell::Cell, fmt};

use bitflags::bitflags;

use crate::codec::{Decoder, Encoder};
use crate::http::body::BodySize;
use crate::http::config::DateService;
use crate::http::error::{DecodeError, EncodeError};
use crate::http::message::ConnectionType;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::http::{Method, Version};
use crate::util::BytesMut;

use super::{decoder, decoder::PayloadType, encoder, Message};

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const HEAD              = 0b0000_0001;
        const STREAM            = 0b0000_0010;
        const KEEPALIVE_ENABLED = 0b0000_0100;
    }
}

/// HTTP/1 Codec
pub struct Codec {
    timer: DateService,
    decoder: decoder::MessageDecoder<Request>,
    version: Cell<Version>,
    ctype: Cell<ConnectionType>,

    // encoder part
    flags: Cell<Flags>,
    encoder: encoder::MessageEncoder<Response<()>>,
}

impl Default for Codec {
    fn default() -> Self {
        Codec::new(DateService::default(), false)
    }
}

impl Clone for Codec {
    fn clone(&self) -> Self {
        Codec {
            timer: self.timer.clone(),
            decoder: self.decoder.clone(),
            version: self.version.clone(),
            ctype: self.ctype.clone(),
            flags: self.flags.clone(),
            encoder: self.encoder.clone(),
        }
    }
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("h1::Codec")
            .field("version", &self.version)
            .field("flags", &self.flags)
            .field("ctype", &self.ctype)
            .field("encoder", &self.encoder)
            .field("decoder", &self.decoder)
            .finish()
    }
}

impl Codec {
    /// Create HTTP/1 codec.
    ///
    /// `keepalive_enabled` how response `connection` header get generated.
    pub fn new(timer: DateService, keep_alive: bool) -> Self {
        let flags = if keep_alive {
            Flags::KEEPALIVE_ENABLED
        } else {
            Flags::empty()
        };

        Codec {
            timer,
            flags: Cell::new(flags),
            decoder: decoder::MessageDecoder::default(),
            version: Cell::new(Version::HTTP_11),
            ctype: Cell::new(ConnectionType::KeepAlive),
            encoder: encoder::MessageEncoder::default(),
        }
    }

    #[inline]
    /// Check if request is upgrade
    pub fn upgrade(&self) -> bool {
        self.ctype.get() == ConnectionType::Upgrade
    }

    #[inline]
    /// Check if last response is keep-alive
    pub fn keepalive(&self) -> bool {
        self.ctype.get() == ConnectionType::KeepAlive
    }

    pub(super) fn set_ctype(&self, ctype: ConnectionType) {
        self.ctype.set(ctype)
    }

    #[inline]
    #[doc(hidden)]
    pub fn set_date_header(&self, dst: &mut BytesMut) {
        self.timer.set_date_header(dst)
    }

    fn insert_flags(&self, f: Flags) {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
    }

    pub(super) fn unset_streaming(&self) {
        let mut flags = self.flags.get();
        flags.remove(Flags::STREAM);
        self.flags.set(flags);
    }
}

impl Decoder for Codec {
    type Item = (Request, PayloadType);
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some((req, payload)) = self.decoder.decode(src)? {
            let head = req.head();
            let mut flags = self.flags.get();
            flags.set(Flags::HEAD, head.method == Method::HEAD);
            self.flags.set(flags);
            self.version.set(head.version);

            let ctype = head.connection_type();
            if ctype == ConnectionType::KeepAlive
                && !flags.contains(Flags::KEEPALIVE_ENABLED)
            {
                self.ctype.set(ConnectionType::Close)
            } else {
                self.ctype.set(ctype)
            }

            if let PayloadType::Stream(_) = payload {
                self.insert_flags(Flags::STREAM)
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

    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            Message::Item((mut res, length)) => {
                // set response version
                res.head_mut().version = self.version.get();

                // connection status
                if let Some(ct) = res.head().ctype() {
                    if ct != ConnectionType::KeepAlive {
                        self.ctype.set(ct)
                    }
                }

                // encode message
                self.encoder.encode(
                    dst,
                    &mut res,
                    self.flags.get().contains(Flags::HEAD),
                    self.flags.get().contains(Flags::STREAM),
                    self.version.get(),
                    length,
                    self.ctype.get(),
                    &self.timer,
                )?;
                // self.headers_size = (dst.len() - len) as u32;
            }
            Message::Chunk(Some(bytes)) => {
                self.encoder.encode_chunk(bytes.as_ref(), dst)?;
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
    use crate::http::{h1::PayloadItem, HttpMessage};
    use crate::util::Bytes;

    #[test]
    fn test_http_request_chunked_payload_and_next_message() {
        let codec = Codec::default();
        assert!(format!("{:?}", codec).contains("h1::Codec"));

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let (req, pl) = codec.decode(&mut buf).unwrap().unwrap();
        let pl = match pl {
            PayloadType::Payload(pl) => pl,
            _ => panic!(),
        };

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

        let codec = Codec::default();
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: upgrade\r\n\r\n",
        );
        let _item = codec.decode(&mut buf).unwrap().unwrap();
        assert!(codec.upgrade());
        assert!(!codec.keepalive());
    }
}
