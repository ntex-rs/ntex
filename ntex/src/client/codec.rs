use std::{cell::Cell, cell::RefCell};

use bitflags::bitflags;

use crate::codec::{Decoder, Encoder};
use crate::http::error::{DecodeError, EncodeError, PayloadError};
use crate::http::h1::{
    Message, MessageType, PayloadDecoder, PayloadItem, PayloadType, decoder, encoder,
};
use crate::http::{ConnectionType, Method, RequestHead, ResponseHead, Version};
use crate::{client::ClientRawRequest, io::IoConfig, util::Bytes, util::BytesMut};

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const HEAD              = 0b0000_0001;
        const KEEPALIVE_ENABLED = 0b0000_1000;
        const STREAM            = 0b0001_0000;
    }
}

#[derive(Debug)]
/// HTTP/1 Codec
pub(crate) struct ClientCodec {
    inner: ClientCodecInner,
}

#[derive(Debug)]
/// HTTP/1 Payload Codec
pub(crate) struct ClientPayloadCodec {
    inner: ClientCodecInner,
}

#[derive(Debug)]
struct ClientCodecInner {
    decoder: decoder::MessageDecoder<ResponseHead>,
    payload: RefCell<Option<PayloadDecoder>>,
    version: Cell<Version>,
    ctype: Cell<ConnectionType>,
    cfg: &'static IoConfig,

    // encoder part
    flags: Cell<Flags>,
    encoder: encoder::MessageEncoder<RequestHead>,
}

impl ClientCodec {
    /// Create HTTP/1 codec.
    ///
    /// `keepalive_enabled` how response `connection` header get generated.
    pub(crate) fn new(keep_alive: bool, cfg: &'static IoConfig) -> Self {
        let flags = if keep_alive {
            Flags::KEEPALIVE_ENABLED
        } else {
            Flags::empty()
        };
        ClientCodec {
            inner: ClientCodecInner {
                cfg,
                decoder: decoder::MessageDecoder::default(),
                payload: RefCell::new(None),
                version: Cell::new(Version::HTTP_11),
                ctype: Cell::new(ConnectionType::Close),
                flags: Cell::new(flags),
                encoder: encoder::MessageEncoder::default(),
            },
        }
    }

    /// Check if last response is keep-alive
    pub(crate) fn keepalive(&self) -> bool {
        self.inner.ctype.get() == ConnectionType::KeepAlive
    }

    /// Check last request's message type
    pub(crate) fn message_type(&self) -> MessageType {
        if self.inner.flags.get().contains(Flags::STREAM) {
            MessageType::Stream
        } else if self.inner.payload.borrow().is_none() {
            MessageType::None
        } else {
            MessageType::Payload
        }
    }

    /// Convert message codec to a payload codec
    pub(crate) fn into_payload_codec(self) -> ClientPayloadCodec {
        ClientPayloadCodec { inner: self.inner }
    }
}

impl ClientPayloadCodec {
    /// Check if last response is keep-alive
    pub(crate) fn keepalive(&self) -> bool {
        self.inner.ctype.get() == ConnectionType::KeepAlive
    }
}

impl Decoder for ClientCodec {
    type Item = ResponseHead;
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        debug_assert!(
            !self.inner.payload.borrow().is_some(),
            "Payload decoder is set"
        );

        if let Some((req, payload)) = self.inner.decoder.decode(src)? {
            if let Some(ctype) = req.ctype() {
                // do not use peer's keep-alive
                if ctype != ConnectionType::KeepAlive {
                    self.inner.ctype.set(ctype);
                }
            }

            if self.inner.flags.get().contains(Flags::HEAD) {
                self.inner.payload.borrow_mut().take();
            } else {
                match payload {
                    PayloadType::None => {
                        self.inner.payload.borrow_mut().take();
                    }
                    PayloadType::Payload(pl) => *self.inner.payload.borrow_mut() = Some(pl),
                    PayloadType::Stream(pl) => {
                        *self.inner.payload.borrow_mut() = Some(pl);
                        let mut flags = self.inner.flags.get();
                        flags.insert(Flags::STREAM);
                        self.inner.flags.set(flags);
                    }
                }
            }
            Ok(Some(req))
        } else {
            Ok(None)
        }
    }
}

impl Decoder for ClientPayloadCodec {
    type Item = Option<Bytes>;
    type Error = PayloadError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        debug_assert!(
            self.inner.payload.borrow().is_some(),
            "Payload decoder is not specified"
        );

        let item = self
            .inner
            .payload
            .borrow_mut()
            .as_mut()
            .unwrap()
            .decode(src)?;

        Ok(match item {
            Some(PayloadItem::Chunk(chunk)) => Some(Some(chunk)),
            Some(PayloadItem::Eof) => {
                self.inner.payload.borrow_mut().take();
                Some(None)
            }
            None => None,
        })
    }
}

impl Encoder for ClientCodec {
    type Item = Message<ClientRawRequest>;
    type Error = EncodeError;

    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        match item {
            Message::Item(mut req) => {
                let inner = &self.inner;
                inner.version.set(req.head.version);
                let mut flags = inner.flags.get();
                flags.set(Flags::HEAD, req.head.method == Method::HEAD);
                inner.flags.set(flags);

                // connection status
                inner.ctype.set(match req.head.connection_type() {
                    ConnectionType::KeepAlive => {
                        if inner.flags.get().contains(Flags::KEEPALIVE_ENABLED) {
                            ConnectionType::KeepAlive
                        } else {
                            ConnectionType::Close
                        }
                    }
                    ConnectionType::Upgrade => ConnectionType::Upgrade,
                    ConnectionType::Close => ConnectionType::Close,
                });

                let headers = req.headers.take();
                inner.encoder.encode(
                    dst,
                    &req.head,
                    false,
                    false,
                    inner.version.get(),
                    req.size,
                    inner.ctype.get(),
                    headers,
                    self.inner.cfg,
                )?;
            }
            Message::Chunk(Some(bytes)) => {
                self.inner
                    .encoder
                    .encode_chunk(bytes.as_ref(), dst, self.inner.cfg)?;
            }
            Message::Chunk(None) => {
                self.inner.encoder.encode_eof(dst)?;
            }
        }
        Ok(())
    }
}
