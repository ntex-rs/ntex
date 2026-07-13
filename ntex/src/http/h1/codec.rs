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
    }
}

/// HTTP/1 Codec
pub struct Codec {
    con_id: usize,
    decoder: decoder::MessageDecoder<Request>,
    version: Cell<Version>,
    ctype: Cell<ConnectionType>,

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
    /// Create HTTP/1 codec.
    ///
    /// `keepalive_enabled` how response `connection` header get generated.
    pub fn new(con_id: usize, cfg: Cfg<HttpServiceConfig>) -> Self {
        let flags = if cfg.ka_enabled {
            Flags::KEEPALIVE_ENABLED
        } else {
            Flags::empty()
        };
        let decoder = decoder::MessageDecoder::new(cfg);

        Codec {
            con_id,
            decoder,
            flags: Cell::new(flags),
            version: Cell::new(Version::HTTP_11),
            ctype: Cell::new(ConnectionType::KeepAlive),
            encoder: encoder::MessageEncoder::default(),
        }
    }

    pub(super) fn is_reading_hdrs(&self) -> bool {
        self.decoder.is_reading_hdrs()
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
            if ctype == ConnectionType::KeepAlive
                && !flags.contains(Flags::KEEPALIVE_ENABLED)
            {
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
                self.encoder.encode(
                    dst,
                    &res,
                    self.flags.get().contains(Flags::HEAD),
                    self.flags.get().contains(Flags::STREAM),
                    self.version.get(),
                    length,
                    self.ctype.get(),
                    None,
                )?;
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
    use crate::{SharedCfg, http::HttpMessage, http::h1::PayloadItem, util::Bytes};

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
    }
}
