//! `permessage-deflate` extension support (RFC 7692).
use std::{cell::Cell, cell::RefCell, fmt, fmt::Write};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::http::header::{HeaderMap, HeaderValue, SEC_WEBSOCKET_EXTENSIONS};
use crate::util::{BufMut, BytesMut};

use super::error::{ProtocolError, WsClientError};

const EXTENSION: &str = "permessage-deflate";
const CLIENT_NO_CONTEXT_TAKEOVER: &str = "client_no_context_takeover";
const SERVER_NO_CONTEXT_TAKEOVER: &str = "server_no_context_takeover";
const CLIENT_MAX_WINDOW_BITS: &str = "client_max_window_bits";
const SERVER_MAX_WINDOW_BITS: &str = "server_max_window_bits";

/// Trailer of a sync flush, removed from the end of each compressed message.
const TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const MIN_WINDOW_BITS: u8 = 8;
const MAX_WINDOW_BITS: u8 = 15;
/// Without context takeover, a compressed message of up to this size is
/// usually not smaller than the message, it is sent uncompressed.
const NO_CONTEXT_MAX_SKIPPED: usize = 32;

/// Parameters of the `permessage-deflate` extension (RFC 7692).
///
/// On the client, this value is the extension offer, set it with
/// [`WsClientConfig::set_deflate()`](super::WsClientConfig::set_deflate). The
/// parameters accepted by the server are available from
/// [`Codec::deflate()`](super::Codec::deflate) of the established connection.
///
/// A [`Codec`](super::Codec) applies the agreed parameters with
/// [`Codec::set_deflate()`](super::Codec::set_deflate).
///
/// Each connection allocates its own compressor and decompressor. With the
/// default window size and compression level the compressor uses about
/// 370KiB, and the decompressor about 46KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeflateConfig {
    level: u32,
    client_no_context_takeover: bool,
    server_no_context_takeover: bool,
    client_max_window_bits: Option<u8>,
    server_max_window_bits: Option<u8>,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl DeflateConfig {
    #[must_use]
    /// Creates a configuration with the default compression level, 32KiB
    /// windows and context takeover in both directions.
    pub fn new() -> Self {
        Self {
            level: Compression::default().level(),
            client_no_context_takeover: false,
            server_no_context_takeover: false,
            client_max_window_bits: None,
            server_max_window_bits: None,
        }
    }

    #[must_use]
    /// Sets the compression level of outgoing messages, from 0 (no
    /// compression) to 9 (best compression).
    ///
    /// The default level is 6. Larger values are capped at 9.
    pub fn set_compression_level(mut self, level: u32) -> Self {
        self.level = level.min(9);
        self
    }

    #[must_use]
    /// Sets the `client_no_context_takeover` parameter.
    ///
    /// The client resets its compressor after each message. It uses less
    /// memory between messages, but repeated content compresses worse.
    pub fn set_client_no_context_takeover(mut self, value: bool) -> Self {
        self.client_no_context_takeover = value;
        self
    }

    #[must_use]
    /// Sets the `server_no_context_takeover` parameter.
    ///
    /// The client asks the server to reset its compressor after each
    /// message. A server that does not agree fails the connection.
    pub fn set_server_no_context_takeover(mut self, value: bool) -> Self {
        self.server_no_context_takeover = value;
        self
    }

    #[must_use]
    /// Sets the `client_max_window_bits` parameter, the base-2 logarithm of
    /// the client's LZ77 window size.
    ///
    /// Values are clamped to the 8 to 15 range, the default window is 15
    /// bits (32KiB). Smaller windows use less memory and compress worse.
    /// A client offers this parameter without a value by default, so the
    /// server can choose a smaller window.
    pub fn set_client_max_window_bits(mut self, bits: u8) -> Self {
        self.client_max_window_bits = Some(bits.clamp(MIN_WINDOW_BITS, MAX_WINDOW_BITS));
        self
    }

    #[must_use]
    /// Sets the `server_max_window_bits` parameter, the base-2 logarithm of
    /// the server's LZ77 window size.
    ///
    /// Values are clamped to the 8 to 15 range. The client asks the server to
    /// use at most this window size, a server that does not agree fails the
    /// connection. By default, the server chooses.
    pub fn set_server_max_window_bits(mut self, bits: u8) -> Self {
        self.server_max_window_bits = Some(bits.clamp(MIN_WINDOW_BITS, MAX_WINDOW_BITS));
        self
    }

    /// Returns the compression level of outgoing messages.
    pub fn compression_level(&self) -> u32 {
        self.level
    }

    /// Returns `true` if the client resets its compressor after each message.
    pub fn client_no_context_takeover(&self) -> bool {
        self.client_no_context_takeover
    }

    /// Returns `true` if the server resets its compressor after each message.
    pub fn server_no_context_takeover(&self) -> bool {
        self.server_no_context_takeover
    }

    /// Returns the client's LZ77 window size, if it is limited.
    pub fn client_max_window_bits(&self) -> Option<u8> {
        self.client_max_window_bits
    }

    /// Returns the server's LZ77 window size, if it is limited.
    pub fn server_max_window_bits(&self) -> Option<u8> {
        self.server_max_window_bits
    }

    /// Creates the `Sec-WebSocket-Extensions` value of a client offer.
    pub(super) fn offer(&self) -> HeaderValue {
        let mut offer = String::from(EXTENSION);
        if self.client_no_context_takeover {
            offer.push_str("; ");
            offer.push_str(CLIENT_NO_CONTEXT_TAKEOVER);
        }
        if self.server_no_context_takeover {
            offer.push_str("; ");
            offer.push_str(SERVER_NO_CONTEXT_TAKEOVER);
        }
        // offered without a value, the server may limit the client's window
        offer.push_str("; ");
        offer.push_str(CLIENT_MAX_WINDOW_BITS);
        if let Some(bits) = self.client_max_window_bits {
            let _ = write!(offer, "={bits}");
        }
        if let Some(bits) = self.server_max_window_bits {
            let _ = write!(offer, "; {SERVER_MAX_WINDOW_BITS}={bits}");
        }
        HeaderValue::try_from(offer).unwrap()
    }

    /// Validates the server's `Sec-WebSocket-Extensions` response to the
    /// offer, returns the agreed parameters.
    ///
    /// The server declines the offer by not including the extension.
    pub(super) fn accept(&self, headers: &HeaderMap) -> Result<Option<Self>, WsClientError> {
        let mut values = headers.get_all(SEC_WEBSOCKET_EXTENSIONS);
        let Some(value) = values.next() else {
            return Ok(None);
        };
        let invalid = || WsClientError::InvalidWebSocketExtensions(value.clone());

        // the client offers one extension, the server accepts it once
        if values.next().is_some() {
            return Err(invalid());
        }
        let value_str = value.to_str().map_err(|_| invalid())?;
        if value_str.contains(',') {
            return Err(invalid());
        }

        let mut params = value_str.split(';').map(str::trim);
        if !params
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(EXTENSION))
        {
            return Err(WsClientError::UnexpectedWebSocketExtensions(value.clone()));
        }

        let mut agreed = DeflateConfig {
            level: self.level,
            // the client may skip context takeover even if the server allows it
            client_no_context_takeover: self.client_no_context_takeover,
            server_no_context_takeover: false,
            client_max_window_bits: self.client_max_window_bits,
            server_max_window_bits: None,
        };
        let (mut client_ctx, mut client_bits) = (false, false);
        for param in params {
            let (name, val) = match param.split_once('=') {
                Some((name, val)) => (name.trim(), Some(unquote(val.trim()).ok_or_else(invalid)?)),
                None => (param, None),
            };

            if name.eq_ignore_ascii_case(CLIENT_NO_CONTEXT_TAKEOVER) {
                if client_ctx || val.is_some() {
                    return Err(invalid());
                }
                client_ctx = true;
                agreed.client_no_context_takeover = true;
            } else if name.eq_ignore_ascii_case(SERVER_NO_CONTEXT_TAKEOVER) {
                if agreed.server_no_context_takeover || val.is_some() {
                    return Err(invalid());
                }
                agreed.server_no_context_takeover = true;
            } else if name.eq_ignore_ascii_case(CLIENT_MAX_WINDOW_BITS) {
                // always offered, the response must carry a value
                let bits = val.and_then(window_bits).ok_or_else(invalid)?;
                if client_bits {
                    return Err(invalid());
                }
                client_bits = true;
                agreed.client_max_window_bits =
                    Some(self.client_max_window_bits.map_or(bits, |b| b.min(bits)));
            } else if name.eq_ignore_ascii_case(SERVER_MAX_WINDOW_BITS) {
                let bits = val.and_then(window_bits).ok_or_else(invalid)?;
                if agreed.server_max_window_bits.is_some()
                    || self.server_max_window_bits.is_some_and(|max| bits > max)
                {
                    return Err(invalid());
                }
                agreed.server_max_window_bits = Some(bits);
            } else {
                return Err(invalid());
            }
        }

        // the server must accept the limits the client asked for
        if (self.server_no_context_takeover && !agreed.server_no_context_takeover)
            || (self.server_max_window_bits.is_some() && agreed.server_max_window_bits.is_none())
        {
            return Err(invalid());
        }
        Ok(Some(agreed))
    }
}

/// Removes the quotes of a quoted-string parameter value.
fn unquote(val: &str) -> Option<&str> {
    match val.strip_prefix('"') {
        Some(val) => val
            .strip_suffix('"')
            .filter(|val| !val.contains(['"', '\\'])),
        None => Some(val),
    }
}

/// Parses a window size, an integer from 8 to 15 without leading zeros.
fn window_bits(val: &str) -> Option<u8> {
    if val.starts_with('0') || !val.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    val.parse::<u8>()
        .ok()
        .filter(|bits| (MIN_WINDOW_BITS..=MAX_WINDOW_BITS).contains(bits))
}

/// Compression state of a connection, shared by the clones of a codec.
pub(super) struct Deflate {
    cfg: DeflateConfig,
    compress: RefCell<Option<Compress>>,
    decompress: RefCell<Option<Decompress>>,
    /// The incoming message has compressed data.
    has_data: Cell<bool>,
}

impl fmt::Debug for Deflate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deflate").field("cfg", &self.cfg).finish()
    }
}

impl Deflate {
    pub(super) fn new(cfg: DeflateConfig) -> Self {
        Self {
            cfg,
            compress: RefCell::new(None),
            decompress: RefCell::new(None),
            has_data: Cell::new(false),
        }
    }

    pub(super) fn config(&self) -> &DeflateConfig {
        &self.cfg
    }

    /// Returns `true` if an unfragmented message of `len` bytes is sent
    /// uncompressed.
    ///
    /// Without context takeover, the compressor is reset after each message,
    /// which costs more than compressing a small message, and the result is
    /// not smaller.
    pub(super) fn is_skipped(&self, len: usize, server: bool) -> bool {
        len <= NO_CONTEXT_MAX_SKIPPED && self.no_context_takeover(server)
    }

    /// Compresses a fragment of an outgoing message.
    ///
    /// The trailer of the last fragment is removed, the compressor is reset
    /// after it if this side does not use context takeover.
    pub(super) fn compress(&self, data: &[u8], fin: bool, server: bool) -> BytesMut {
        let mut compress = self.compress.borrow_mut();
        let compress = compress.get_or_insert_with(|| {
            let bits = if server {
                self.cfg.server_max_window_bits
            } else {
                self.cfg.client_max_window_bits
            }
            .unwrap_or(MAX_WINDOW_BITS);

            // The 8-bit window is not supported by zlib, stored blocks do
            // not refer to previous data and fit any window.
            let level = if bits == MIN_WINDOW_BITS {
                Compression::none()
            } else {
                Compression::new(self.cfg.level)
            };
            Compress::new_with_window_bits(level, false, bits.max(MIN_WINDOW_BITS + 1))
        });

        let mut out = BytesMut::with_capacity(data.len() / 2 + 64);
        let mut data = data;
        loop {
            let (total_in, total_out) = (compress.total_in(), compress.total_out());
            // SAFETY: the compressor only writes to the slice, and
            // `advance_mut` covers just the bytes it has written
            unsafe {
                compress
                    .compress_uninit(
                        data,
                        out.chunk_mut().as_uninit_slice_mut(),
                        FlushCompress::Sync,
                    )
                    .expect("deflate stream is valid");
                out.advance_mut(usize::try_from(compress.total_out() - total_out).unwrap());
            }
            data = &data[usize::try_from(compress.total_in() - total_in).unwrap()..];

            // the flush is complete once it leaves space in the buffer
            if data.is_empty() && out.remaining_mut() > 0 {
                break;
            }
            out.reserve(out.capacity().max(64));
        }

        if fin {
            if out.ends_with(&TAIL) {
                out.truncate(out.len() - TAIL.len());
            } else if out.is_empty() {
                // Nothing is written after a previous flush. With the
                // trailer, a single zero byte is an empty stored block.
                out.put_u8(0x00);
            }
            if self.no_context_takeover(server) {
                compress.reset();
            }
        }
        out
    }

    /// Decompresses a fragment of an incoming message, the output is limited
    /// to `max_size` bytes.
    pub(super) fn decompress(
        &self,
        data: &[u8],
        fin: bool,
        server: bool,
        max_size: usize,
    ) -> Result<BytesMut, ProtocolError> {
        let mut decompress = self.decompress.borrow_mut();
        // a smaller window of the peer fits the maximum window
        let decompress = decompress.get_or_insert_with(|| Decompress::new(false));

        // The trailer is the length of an empty stored block, the block
        // header ends the compressed data. A message without compressed data
        // has no block header, the peer did not write the empty block.
        let has_data = self.has_data.get() || !data.is_empty();
        self.has_data.set(has_data && !fin);

        let mut out =
            BytesMut::with_capacity((data.len() * 2 + 64).min(max_size.saturating_add(1)));
        let result = inflate(decompress, data, &mut out, max_size).and_then(|()| {
            if fin && has_data {
                inflate(decompress, &TAIL, &mut out, max_size)
            } else {
                Ok(())
            }
        });
        if result.is_err() {
            self.has_data.set(false);
            decompress.reset(false);
            return result.map(|()| out);
        }

        if fin && self.no_context_takeover(!server) {
            decompress.reset(false);
        }
        Ok(out)
    }

    fn no_context_takeover(&self, server: bool) -> bool {
        if server {
            self.cfg.server_no_context_takeover
        } else {
            self.cfg.client_no_context_takeover
        }
    }
}

fn inflate(
    decompress: &mut Decompress,
    mut data: &[u8],
    out: &mut BytesMut,
    max_size: usize,
) -> Result<(), ProtocolError> {
    loop {
        if out.remaining_mut() == 0 {
            if out.len() > max_size {
                return Err(ProtocolError::Overflow);
            }
            out.reserve_exact(out.capacity().max(64).min(max_size + 1 - out.len()));
        }

        let (total_in, total_out) = (decompress.total_in(), decompress.total_out());
        // SAFETY: the decompressor only writes to the slice, and
        // `advance_mut` covers just the bytes it has written
        let status = unsafe {
            let status = decompress
                .decompress_uninit(
                    data,
                    out.chunk_mut().as_uninit_slice_mut(),
                    FlushDecompress::Sync,
                )
                .map_err(|_| ProtocolError::InvalidCompressedData)?;
            out.advance_mut(usize::try_from(decompress.total_out() - total_out).unwrap());
            status
        };
        let read = usize::try_from(decompress.total_in() - total_in).unwrap();
        let written = decompress.total_out() - total_out;
        data = &data[read..];

        if out.len() > max_size {
            return Err(ProtocolError::Overflow);
        }
        if status == Status::StreamEnd {
            // a block with BFINAL set ends the stream, the peer starts a new
            // one with an empty window
            decompress.reset(false);
            if data.is_empty() {
                return Ok(());
            }
        } else if out.remaining_mut() > 0 {
            // all output is written once it leaves space in the buffer
            if data.is_empty() {
                return Ok(());
            } else if read == 0 && written == 0 {
                return Err(ProtocolError::InvalidCompressedData);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(values: &[&'static str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_static(value));
        }
        headers
    }

    fn accept(
        offer: &DeflateConfig,
        value: &'static str,
    ) -> Result<Option<DeflateConfig>, WsClientError> {
        offer.accept(&response(&[value]))
    }

    fn invalid(offer: &DeflateConfig, value: &'static str) -> bool {
        matches!(
            accept(offer, value),
            Err(WsClientError::InvalidWebSocketExtensions(val)) if val == value
        )
    }

    #[test]
    fn config() {
        let cfg = DeflateConfig::default();
        assert_eq!(cfg, DeflateConfig::new());
        assert_eq!(cfg.compression_level(), 6);
        assert!(!cfg.client_no_context_takeover());
        assert!(!cfg.server_no_context_takeover());
        assert_eq!(cfg.client_max_window_bits(), None);
        assert_eq!(cfg.server_max_window_bits(), None);

        let cfg = cfg
            .set_compression_level(10)
            .set_client_no_context_takeover(true)
            .set_server_no_context_takeover(true)
            .set_client_max_window_bits(7)
            .set_server_max_window_bits(16);
        assert_eq!(cfg.compression_level(), 9);
        assert!(cfg.client_no_context_takeover());
        assert!(cfg.server_no_context_takeover());
        assert_eq!(cfg.client_max_window_bits(), Some(8));
        assert_eq!(cfg.server_max_window_bits(), Some(15));
        assert!(format!("{:?}", Deflate::new(cfg)).contains("Deflate"));
    }

    #[test]
    fn offer() {
        assert_eq!(
            DeflateConfig::new().offer(),
            "permessage-deflate; client_max_window_bits"
        );
        assert_eq!(
            DeflateConfig::new()
                .set_client_no_context_takeover(true)
                .set_server_no_context_takeover(true)
                .set_client_max_window_bits(10)
                .set_server_max_window_bits(12)
                .offer(),
            "permessage-deflate; client_no_context_takeover; server_no_context_takeover; \
             client_max_window_bits=10; server_max_window_bits=12"
        );
    }

    #[test]
    fn accept_declined() {
        let offer = DeflateConfig::new();
        assert_eq!(offer.accept(&HeaderMap::new()).unwrap(), None);
    }

    #[test]
    fn accept_params() {
        let offer = DeflateConfig::new().set_compression_level(1);
        let agreed = accept(&offer, "permessage-deflate").unwrap().unwrap();
        assert_eq!(agreed, offer);

        let agreed = accept(
            &offer,
            "Permessage-Deflate ; Client_No_Context_Takeover;server_no_context_takeover ; \
             client_max_window_bits = 10; server_max_window_bits=\"9\"",
        )
        .unwrap()
        .unwrap();
        assert_eq!(agreed.compression_level(), 1);
        assert!(agreed.client_no_context_takeover());
        assert!(agreed.server_no_context_takeover());
        assert_eq!(agreed.client_max_window_bits(), Some(10));
        assert_eq!(agreed.server_max_window_bits(), Some(9));

        // the client keeps its own limits
        let offer = DeflateConfig::new()
            .set_client_no_context_takeover(true)
            .set_client_max_window_bits(9);
        let agreed = accept(&offer, "permessage-deflate; client_max_window_bits=12")
            .unwrap()
            .unwrap();
        assert!(agreed.client_no_context_takeover());
        assert_eq!(agreed.client_max_window_bits(), Some(9));
        let agreed = accept(&offer, "permessage-deflate").unwrap().unwrap();
        assert_eq!(agreed.client_max_window_bits(), Some(9));

        // the server accepts the client's limits
        let offer = DeflateConfig::new()
            .set_server_no_context_takeover(true)
            .set_server_max_window_bits(10);
        let agreed = accept(
            &offer,
            "permessage-deflate; server_no_context_takeover; server_max_window_bits=8",
        )
        .unwrap()
        .unwrap();
        assert!(agreed.server_no_context_takeover());
        assert_eq!(agreed.server_max_window_bits(), Some(8));
        let agreed = accept(
            &offer,
            "permessage-deflate; server_no_context_takeover; server_max_window_bits=10",
        )
        .unwrap()
        .unwrap();
        assert_eq!(agreed.server_max_window_bits(), Some(10));
    }

    #[test]
    fn accept_errors() {
        let offer = DeflateConfig::new();

        // other extensions are not offered
        for value in ["x-webkit-deflate-frame", "permessage-deflate-x", ""] {
            assert!(matches!(
                accept(&offer, value),
                Err(WsClientError::UnexpectedWebSocketExtensions(val)) if val == value
            ));
        }

        for value in [
            // more than one extension
            "permessage-deflate, permessage-deflate",
            "permessage-deflate, x-ext",
            // unknown or malformed parameters
            "permessage-deflate; unknown",
            "permessage-deflate; unknown=1",
            "permessage-deflate;",
            "permessage-deflate; client_no_context_takeover=1",
            "permessage-deflate; server_no_context_takeover=\"\"",
            // duplicated parameters
            "permessage-deflate; client_no_context_takeover; client_no_context_takeover",
            "permessage-deflate; server_no_context_takeover; server_no_context_takeover",
            "permessage-deflate; client_max_window_bits=10; client_max_window_bits=10",
            "permessage-deflate; server_max_window_bits=10; server_max_window_bits=10",
            // invalid window sizes
            "permessage-deflate; client_max_window_bits",
            "permessage-deflate; server_max_window_bits",
            "permessage-deflate; client_max_window_bits=7",
            "permessage-deflate; client_max_window_bits=16",
            "permessage-deflate; server_max_window_bits=09",
            "permessage-deflate; server_max_window_bits=+9",
            "permessage-deflate; server_max_window_bits=1x",
            "permessage-deflate; server_max_window_bits=256",
            "permessage-deflate; server_max_window_bits=\"10",
            "permessage-deflate; server_max_window_bits=\"1\\0\"",
        ] {
            assert!(invalid(&offer, value), "{value}");
        }

        // the server must accept the requested limits
        let offer = DeflateConfig::new()
            .set_server_no_context_takeover(true)
            .set_server_max_window_bits(10);
        for value in [
            "permessage-deflate; server_max_window_bits=10",
            "permessage-deflate; server_no_context_takeover",
            "permessage-deflate; server_no_context_takeover; server_max_window_bits=11",
        ] {
            assert!(invalid(&offer, value), "{value}");
        }

        // the extension is accepted once
        let headers = response(&["permessage-deflate", "permessage-deflate"]);
        assert!(matches!(
            offer.accept(&headers),
            Err(WsClientError::InvalidWebSocketExtensions(_))
        ));
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_EXTENSIONS,
            HeaderValue::from_bytes(b"permessage-deflate; \xff").unwrap(),
        );
        assert!(matches!(
            offer.accept(&headers),
            Err(WsClientError::InvalidWebSocketExtensions(_))
        ));
    }

    #[test]
    fn compress_roundtrip() {
        let server = Deflate::new(DeflateConfig::new());
        let client = Deflate::new(DeflateConfig::new());

        let msg = b"Hello, Hello, Hello, Hello";
        let first = server.compress(msg, true, true);
        assert!(first.len() < msg.len());
        assert!(!first.ends_with(&TAIL));
        assert_eq!(client.decompress(&first, true, false, 1024).unwrap(), msg);

        // the second message refers to the first one
        let second = server.compress(msg, true, true);
        assert!(second.len() < first.len());
        assert_eq!(client.decompress(&second, true, false, 1024).unwrap(), msg);

        // empty and incompressible messages
        let empty = server.compress(b"", true, true);
        assert_eq!(client.decompress(&empty, true, false, 1024).unwrap(), b"");
        let data: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let compressed = server.compress(&data, true, true);
        assert_eq!(
            client
                .decompress(&compressed, true, false, data.len())
                .unwrap(),
            data
        );
        assert!(matches!(
            client.decompress(
                &server.compress(&data, true, true),
                true,
                false,
                data.len() - 1
            ),
            Err(ProtocolError::Overflow)
        ));
    }

    #[test]
    fn compress_fragments() {
        let server = Deflate::new(DeflateConfig::new());
        let client = Deflate::new(DeflateConfig::new());

        let mut out = Vec::new();
        for (data, fin) in [(&b"Hello, "[..], false), (b"", false), (b"Hello", true)] {
            let compressed = server.compress(data, fin, true);
            // only the end of the message drops the trailer, an empty
            // fragment is empty after a flush
            assert_eq!(compressed.ends_with(&TAIL), !fin && !data.is_empty());
            assert_eq!(compressed.is_empty(), data.is_empty());
            out.extend(client.decompress(&compressed, fin, false, 1024).unwrap());
        }
        assert_eq!(out, b"Hello, Hello");

        // the last fragment is empty
        let mut out = Vec::new();
        for (data, fin) in [(&b"Hello"[..], false), (b"", true)] {
            let compressed = server.compress(data, fin, true);
            out.extend(client.decompress(&compressed, fin, false, 1024).unwrap());
        }
        assert_eq!(out, b"Hello");
        let compressed = server.compress(b"Hello", true, true);
        assert_eq!(
            client.decompress(&compressed, true, false, 1024).unwrap(),
            b"Hello"
        );
    }

    #[test]
    fn empty_last_fragment() {
        // a compressed payload split into fragments, the last one is empty
        let server = Deflate::new(DeflateConfig::new());
        let client = Deflate::new(DeflateConfig::new());
        for _ in 0..3 {
            let compressed = server.compress(&b"Hello, Hello, Hello".repeat(50), true, true);
            let mut out = Vec::new();
            for chunk in compressed.chunks(7) {
                out.extend(client.decompress(chunk, false, false, 4096).unwrap());
            }
            out.extend(client.decompress(&[], true, false, 4096).unwrap());
            assert_eq!(out, b"Hello, Hello, Hello".repeat(50));
        }

        // a message without compressed data
        assert_eq!(client.decompress(&[], false, false, 4096).unwrap(), b"");
        assert_eq!(client.decompress(&[], true, false, 4096).unwrap(), b"");
        let compressed = server.compress(b"Hello", true, true);
        assert_eq!(
            client.decompress(&compressed, true, false, 4096).unwrap(),
            b"Hello"
        );
    }

    #[test]
    fn empty_messages() {
        let server = Deflate::new(DeflateConfig::new());
        let client = Deflate::new(DeflateConfig::new());

        // RFC 7692 7.2.3.6, an empty message is a single zero byte
        assert_eq!(server.compress(b"", true, true), [0x00]);
        assert_eq!(server.compress(b"", true, true), [0x00]);
        let compressed = server.compress(b"Hello", true, true);
        assert_eq!(server.compress(b"", true, true), [0x00]);
        assert_eq!(server.compress(b"", true, true), [0x00]);

        for _ in 0..2 {
            assert_eq!(client.decompress(&[0x00], true, false, 1024).unwrap(), b"");
        }
        assert_eq!(
            client.decompress(&compressed, true, false, 1024).unwrap(),
            b"Hello"
        );
        // an empty payload from the peer does not consume the next message
        for _ in 0..2 {
            assert_eq!(client.decompress(&[], true, false, 1024).unwrap(), b"");
        }
        let compressed = server.compress(b"Hello", true, true);
        assert_eq!(
            client.decompress(&compressed, true, false, 1024).unwrap(),
            b"Hello"
        );
    }

    #[test]
    fn no_context_takeover() {
        let cfg = DeflateConfig::new().set_server_no_context_takeover(true);
        let server = Deflate::new(cfg.clone());
        let client = Deflate::new(cfg);

        // the server resets its compressor, and the client its decompressor
        let first = server.compress(b"Hello", true, true);
        assert_eq!(server.compress(b"Hello", true, true), first);
        assert_eq!(
            client.decompress(&first, true, false, 1024).unwrap(),
            b"Hello"
        );
        assert_eq!(
            client.decompress(&first, true, false, 1024).unwrap(),
            b"Hello"
        );

        // the client keeps its context
        let first = client.compress(b"Hello", true, false);
        assert_ne!(client.compress(b"Hello", true, false), first);
    }

    #[test]
    fn window_bits() {
        let data: Vec<u8> = b"0123456789abcdef".repeat(4096);
        for bits in 8..=15 {
            let cfg = DeflateConfig::new()
                .set_client_max_window_bits(bits)
                .set_server_max_window_bits(bits);
            let server = Deflate::new(cfg.clone());
            let client = Deflate::new(cfg);
            for _ in 0..2 {
                let compressed = client.compress(&data, true, false);
                // stored blocks only, they fit the 8-bit window
                assert_eq!(compressed.len() > data.len(), bits == 8, "{bits}");
                assert_eq!(
                    server
                        .decompress(&compressed, true, true, data.len())
                        .unwrap(),
                    data
                );
            }
        }
    }

    #[test]
    fn invalid_data() {
        let client = Deflate::new(DeflateConfig::new());
        // a reserved block type
        assert!(matches!(
            client.decompress(&[0xff, 0xff], true, false, 1024),
            Err(ProtocolError::InvalidCompressedData)
        ));
        // the decompressor is reset after an error
        assert_eq!(
            client
                .decompress(
                    &[0xf2, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00],
                    true,
                    false,
                    1024
                )
                .unwrap(),
            b"Hello"
        );
        // a distance beyond the start of the stream
        let client = Deflate::new(DeflateConfig::new());
        assert!(matches!(
            client.decompress(&[0xf2, 0x00, 0x11, 0x00, 0x00], true, false, 1024),
            Err(ProtocolError::InvalidCompressedData)
        ));
    }
}
