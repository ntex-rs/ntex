//! Binary-stream adaptation for WebSocket connections.
use std::{cell::Cell, io, task::Poll};

use crate::codec::{Decoder, Encoder};
use crate::io::{Filter, FilterBuf, FilterLayer, Io, Layer};
use crate::service::{Ctx, Service};

use super::{CloseCode, CloseReason, Codec, Frame, Item, Message};

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8  {
        const CLOSED       = 0b0001;
        const PEER_CLOSED  = 0b0010;
        const PROTO_ERR    = 0b0100;
    }
}

#[derive(Clone, Debug)]
/// I/O filter that exposes binary WebSocket messages as a byte stream.
///
/// Incoming binary messages and fragments are forwarded as bytes. Text
/// messages are rejected, ping frames receive automatic pong replies, and
/// close frames close the underlying I/O stream.
pub struct WsTransport {
    codec: Codec,
    flags: Cell<Flags>,
}

impl WsTransport {
    /// Adds a binary WebSocket transport filter to `io`.
    pub fn create<F: Filter>(io: Io<F>, codec: Codec) -> Io<Layer<WsTransport, F>> {
        io.add_filter(WsTransport {
            codec,
            flags: Cell::new(Flags::empty()),
        })
    }

    fn insert_flags(&self, flags: Flags) {
        let mut f = self.flags.get();
        f.insert(flags);
        self.flags.set(f);
    }

    fn send_close(&self, buf: &FilterBuf<'_>, code: CloseCode) {
        if !self.flags.get().contains(Flags::CLOSED) {
            self.insert_flags(Flags::CLOSED);
            let _ = buf.with_write_buffers(|_, w_dst| {
                self.codec.encode(
                    Message::Close(Some(CloseReason {
                        code,
                        description: None,
                    })),
                    w_dst,
                )
            });
        }
    }

    /// Fails the connection: sends a protocol error close frame and starts a
    /// graceful shutdown, so the frame is delivered before the connection
    /// is closed with `err`.
    fn fail(&self, buf: &FilterBuf<'_>, err: io::Error) -> io::Error {
        self.insert_flags(Flags::PROTO_ERR);
        self.send_close(buf, CloseCode::Protocol);
        buf.io().close();
        err
    }
}

impl FilterLayer for WsTransport {
    #[inline]
    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        self.send_close(buf, CloseCode::Normal);

        // Wait for the peer's close frame. It cannot arrive after read eof,
        // and a failed connection is not required to wait for it.
        let flags = self.flags.get();
        if flags.intersects(Flags::PEER_CLOSED | Flags::PROTO_ERR) || buf.io().is_read_eof() {
            Ok(Poll::Ready(()))
        } else {
            Ok(Poll::Pending)
        }
    }

    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_read_buffers(|r_src, dst| {
            if let Some(src) = r_src {
                loop {
                    let Some(frame) = self.codec.decode(src).map_err(|e| {
                        log::trace!("Failed to decode ws codec frames: {e:?}");
                        self.fail(buf, io::Error::new(io::ErrorKind::InvalidData, e))
                    })?
                    else {
                        break;
                    };

                    match frame {
                        // the codec enforces fragment ordering
                        Frame::Binary(bin)
                        | Frame::Continuation(
                            Item::FirstBinary(bin) | Item::Continue(bin) | Item::Last(bin),
                        ) => dst.extend_from_slice(&bin),
                        Frame::Continuation(Item::FirstText(_)) => {
                            return Err(self.fail(
                                buf,
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "WebSocket Text continuation frames are not supported",
                                ),
                            ));
                        }
                        Frame::Text(_) => {
                            return Err(self.fail(
                                buf,
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "WebSockets Text frames are not supported",
                                ),
                            ));
                        }
                        Frame::Ping(msg) => {
                            buf.with_write_buffers(|_, w_dst| {
                                let _ = self.codec.encode(Message::Pong(msg), w_dst);
                            });
                        }
                        Frame::Pong(_) => (),
                        Frame::Close(_) => {
                            self.insert_flags(Flags::PEER_CLOSED);
                            buf.io().close();
                            break;
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|w_src, w_dst| -> Result<(), super::error::ProtocolError> {
            if self.flags.get().contains(Flags::CLOSED) {
                // nothing can be sent after the close frame
                w_src.clear();
                return Ok(());
            }
            while let Some(page) = w_src.take() {
                self.codec.encode_page(page, w_dst)?;
            }
            Ok(())
        })
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }
}

#[derive(Clone, Debug)]
/// Service that adds a [`WsTransport`] filter to an I/O stream.
pub struct WsTransportService {
    codec: Codec,
}

impl WsTransportService {
    /// Creates a transport service using `codec`.
    pub fn new(codec: Codec) -> Self {
        Self { codec }
    }
}

impl<F: Filter> Service<(), Io<F>> for WsTransportService {
    type Res = Io<Layer<WsTransport, F>>;
    type Error = io::Error;

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        Ok(WsTransport::create(io, self.codec.clone()))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::io::testing::IoTest;
    use crate::time::{Millis, sleep};
    use crate::util::{BytePages, Bytes, BytesMut};

    #[derive(Debug)]
    struct Passthrough;

    impl FilterLayer for Passthrough {
        fn process_read_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_read_buffers(|src, dst| {
                if let Some(src) = src {
                    dst.extend_from_slice(src);
                    src.clear();
                }
            });
            Ok(())
        }

        fn process_write_buf(&self, buf: &FilterBuf<'_>) -> io::Result<()> {
            buf.with_write_buffers(BytePages::move_to);
            Ok(())
        }
    }

    fn peer_close() -> Bytes {
        let mut dst = BytePages::default();
        Codec::new()
            .set_client_mode()
            .encodev(Message::Close(None), &mut dst)
            .unwrap();
        Bytes::from(dst)
    }

    fn start_shutdown<F: Filter>(io: Io<F>) -> Rc<Cell<bool>> {
        let done = Rc::new(Cell::new(false));
        let done2 = done.clone();
        crate::rt::spawn(async move {
            let _ = io.shutdown().await;
            done2.set(true);
        });
        done
    }

    fn assert_close_sent(client: &IoTest) {
        assert_close_code(client, CloseCode::Normal);
    }

    fn assert_close_code(client: &IoTest, code: CloseCode) {
        let mut data = BytesMut::from(&client.read_any()[..]);
        assert_eq!(
            Codec::new().set_client_mode().decode(&mut data).unwrap(),
            Some(Frame::Close(Some(code.into())))
        );
    }

    async fn protocol_error_sends_close<F: Filter>(client: IoTest, io: Io<F>, input: Bytes) {
        client.remote_buffer_cap(1024);
        let io = WsTransport::create(io, Codec::new());

        client.write(input);
        let err = io.recv(&crate::codec::BytesCodec).await.unwrap_err();
        assert_eq!(err.into_inner().kind(), io::ErrorKind::InvalidData);
        sleep(Millis(50)).await;

        assert_close_code(&client, CloseCode::Protocol);
        assert!(io.is_closed());
    }

    #[crate::rt_test]
    async fn invalid_frame_sends_protocol_close() {
        // an unmasked frame from a client
        let (client, server) = IoTest::create();
        let input = Bytes::from_static(&[0x82, 0x01, 0x00]);
        protocol_error_sends_close(client, Io::from(server), input).await;
    }

    #[crate::rt_test]
    async fn invalid_frame_sends_protocol_close_over_inner_filter() {
        let (client, server) = IoTest::create();
        let io = Io::from(server).add_filter(Passthrough);
        let input = Bytes::from_static(&[0x82, 0x01, 0x00]);
        protocol_error_sends_close(client, io, input).await;
    }

    #[crate::rt_test]
    async fn text_frame_sends_protocol_close() {
        let (client, server) = IoTest::create();
        let mut input = BytePages::default();
        Codec::new()
            .set_client_mode()
            .encodev(Message::Text("text".into()), &mut input)
            .unwrap();
        protocol_error_sends_close(client, Io::from(server), Bytes::from(input)).await;
    }

    async fn shutdown_waits_for_peer_close<F: Filter>(client: IoTest, io: Io<F>) {
        client.remote_buffer_cap(1024);
        let io = WsTransport::create(io, Codec::new());
        let done = start_shutdown(io);
        sleep(Millis(50)).await;

        assert_close_sent(&client);
        assert!(!done.get());

        client.write(peer_close());
        sleep(Millis(50)).await;
        assert!(done.get());
    }

    #[crate::rt_test]
    async fn shutdown_waits_for_close_reply() {
        let (client, server) = IoTest::create();
        shutdown_waits_for_peer_close(client, Io::from(server)).await;
    }

    #[crate::rt_test]
    async fn shutdown_waits_for_close_reply_over_inner_filter() {
        let (client, server) = IoTest::create();
        shutdown_waits_for_peer_close(client, Io::from(server).add_filter(Passthrough)).await;
    }

    #[crate::rt_test]
    async fn shutdown_completes_on_read_eof() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let done = start_shutdown(WsTransport::create(Io::from(server), Codec::new()));
        sleep(Millis(50)).await;
        assert_close_sent(&client);
        assert!(!done.get());

        client.close().await;
        sleep(Millis(50)).await;
        assert!(done.get());
    }

    #[crate::rt_test]
    async fn peer_close_completes_shutdown() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1024);
        let io = WsTransport::create(Io::from(server), Codec::new());

        client.write(peer_close());
        assert!(io.recv(&crate::codec::BytesCodec).await.unwrap().is_none());
        sleep(Millis(50)).await;

        assert_close_sent(&client);
        assert!(io.is_closed());
    }
}
