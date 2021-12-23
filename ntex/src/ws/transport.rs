//! WS container
use std::{any, cell::Cell, io, task::Context, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Filter, FilterFactory, Io, ReadStatus, WriteStatus};
use crate::util::{BufMut, BytesMut, PoolRef, Ready};

use super::{Codec, Frame, Item, Message};

bitflags::bitflags! {
    #[derive(Display)]
    struct Flags: u8 {
        const CLOSED = 0b0000_0001;
        const CONTINUATION = 0b0000_0010;
    }
}

/// WebSockets transport
///
/// Allows to use websocket connection as io stream
pub struct WsTransport<F> {
    inner: F,
    codec: Codec,
    read_buf: Cell<Option<BytesMut>>,
    flags: Cell<Flags>,
    pool: PoolRef,
}

impl<F> WsTransport<F> {
    #[inline]
    pub fn new(inner: F, codec: Codec, pool: PoolRef) -> WsTransport<F> {
        Self {
            inner,
            codec,
            pool,
            read_buf: Cell::new(None),
            flags: Cell::new(Flags::empty()),
        }
    }

    fn remove_flags(&self, f: Flags) -> Flags {
        let mut flags = self.flags.get();
        flags.remove(f);
        self.flags.set(flags);
        flags
    }

    fn insert_flags(&self, f: Flags) -> Flags {
        let mut flags = self.flags.get();
        flags.insert(f);
        self.flags.set(flags);
        flags
    }
}

impl<F: Filter> Filter for WsTransport<F> {
    #[inline]
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        self.inner.query(id)
    }

    #[inline]
    fn want_read(&self) {
        self.inner.want_read()
    }

    #[inline]
    fn want_shutdown(&self) {
        self.inner.want_shutdown()
    }

    #[inline]
    fn poll_shutdown(&self) -> Poll<io::Result<()>> {
        self.inner.poll_shutdown()
    }

    #[inline]
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        self.inner.poll_read_ready(cx)
    }

    #[inline]
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        self.inner.poll_write_ready(cx)
    }

    #[inline]
    fn closed(&self, err: Option<io::Error>) {
        self.inner.closed(err)
    }

    #[inline]
    fn get_read_buf(&self) -> Option<BytesMut> {
        self.read_buf.take()
    }

    #[inline]
    fn get_write_buf(&self) -> Option<BytesMut> {
        None
    }

    fn release_read_buf(&self, mut src: BytesMut, nbytes: usize) -> Result<(), io::Error> {
        if nbytes == 0 {
            if !src.is_empty() {
                self.read_buf.set(Some(src));
            }
            Ok(())
        } else {
            let (hw, lw) = self.pool.read_params().unpack();

            // get inner filter buffer
            let mut buf = if let Some(buf) = self.inner.get_read_buf() {
                buf
            } else {
                self.pool.get_read_buf()
            };
            let len = buf.len();
            let mut flags = self.flags.get();

            // read from input buffer
            loop {
                let result = self.codec.decode(&mut src).map_err(|e| {
                    log::trace!("ws codec failed to decode bytes stream: {:?}", e);
                    io::Error::new(io::ErrorKind::Other, e)
                })?;

                // make sure we've got room
                let remaining = buf.remaining_mut();
                if remaining < lw {
                    buf.reserve(hw - remaining);
                }

                match result {
                    Some(frame) => match frame {
                        Frame::Binary(bin) => buf.extend_from_slice(&bin),
                        Frame::Continuation(item) => match item {
                            Item::FirstText(_) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    "WebSocket text continuation frames are not supported",
                                ));
                            }
                            Item::FirstBinary(bin) => {
                                flags = self.insert_flags(Flags::CONTINUATION);
                                buf.extend_from_slice(&bin);
                            }
                            Item::Continue(bin) => {
                                if flags.contains(Flags::CONTINUATION) {
                                    buf.extend_from_slice(&bin);
                                } else {
                                    return Err(io::Error::new(
                                        io::ErrorKind::Other,
                                        "Continuation frame must follow data frame with FIN bit clear",
                                    ));
                                }
                            }
                            Item::Last(bin) => {
                                if flags.contains(Flags::CONTINUATION) {
                                    flags = self.remove_flags(Flags::CONTINUATION);
                                    buf.extend_from_slice(&bin);
                                } else {
                                    return Err(io::Error::new(
                                        io::ErrorKind::Other,
                                        "Received last frame without initial continuation frame",
                                    ));
                                }
                            }
                        },
                        Frame::Text(_) => {
                            log::trace!("WebSocket text frames are not supported");
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                "WebSocket text frames are not supported",
                            ));
                        }
                        Frame::Ping(msg) => {
                            let mut b = self
                                .inner
                                .get_write_buf()
                                .unwrap_or_else(|| self.pool.get_write_buf());
                            self.codec
                                .encode(Message::Pong(msg), &mut b)
                                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                            self.release_write_buf(b)?;
                        }
                        Frame::Pong(_) => (),
                        Frame::Close(_) => {
                            let mut b = self
                                .inner
                                .get_write_buf()
                                .unwrap_or_else(|| self.pool.get_write_buf());
                            self.codec
                                .encode(Message::Close(None), &mut b)
                                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                            self.release_write_buf(b)?;
                            break;
                        }
                    },
                    None => break,
                }
            }

            if !src.is_empty() {
                self.read_buf.set(Some(src));
            } else {
                self.pool.release_read_buf(src);
            }
            let new_bytes = buf.len() - len;
            self.inner.release_read_buf(buf, new_bytes)
        }
    }

    fn release_write_buf(&self, src: BytesMut) -> Result<(), io::Error> {
        let mut buf = if let Some(mut buf) = self.inner.get_write_buf() {
            // make sure we've got room
            let (hw, lw) = self.pool.write_params().unpack();
            let remaining = buf.remaining_mut();
            if remaining < lw {
                buf.reserve(hw - remaining);
            }
            buf
        } else {
            self.pool.get_write_buf()
        };
        self.codec
            .encode(Message::Binary(src.freeze()), &mut buf)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "Cannot encode ws frame"))?;
        self.inner.release_write_buf(buf)
    }
}

/// WebSockets transport factory
pub struct WsTransportFactory {
    codec: Codec,
}

impl WsTransportFactory {
    #[inline]
    /// Create ws filter factory
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }
}

impl<F: Filter> FilterFactory<F> for WsTransportFactory {
    type Filter = WsTransport<F>;

    type Error = io::Error;
    type Future = Ready<Io<Self::Filter>, Self::Error>;

    fn create(self, st: Io<F>) -> Self::Future {
        let pool = st.memory_pool();
        Ready::from(st.map_filter(|inner| Ok(WsTransport::new(inner, self.codec, pool))))
    }
}
