#![allow(clippy::unused_async)]
use std::rc::Rc;

use crate::{Cfg, io::IoRef, io::OnDisconnect, rt, time::sleep, util::select, ws};

#[derive(Clone, Debug)]
/// A clonable handle for sending messages over a WebSocket connection.
pub struct WsSink(Rc<WsSinkInner>);

#[derive(Debug)]
struct WsSinkInner {
    io: IoRef,
    codec: ws::Codec,
    cfg: Cfg<ws::WsClientConfig>,
}

impl WsSink {
    pub(crate) fn new(io: IoRef, codec: ws::Codec, cfg: Cfg<ws::WsClientConfig>) -> Self {
        Self(Rc::new(WsSinkInner { io, codec, cfg }))
    }

    /// Returns the underlying I/O handle.
    pub fn io(&self) -> &IoRef {
        &self.0.io
    }

    /// Encodes and queues a message for the peer.
    ///
    /// Sending a close message starts the closing handshake. The connection
    /// remains open for the peer's close response and is shut down when the
    /// configured closing-handshake timeout expires.
    pub async fn send(&self, item: ws::Message) -> Result<(), ws::error::ProtocolError> {
        let close = matches!(item, ws::Message::Close(_));

        if let Err(e) = self.0.io.encode(item, &self.0.codec) {
            Err(e)
        } else {
            if close && self.0.cfg.close_timeout.non_zero() {
                let io = self.0.io.clone();
                let close_timeout = self.0.cfg.close_timeout;
                rt::spawn(async move {
                    select(sleep(close_timeout), io.on_disconnect()).await;
                    if !io.is_closed() {
                        io.close();
                    }
                });
            }
            Ok(())
        }
    }

    /// Returns a future that resolves when the connection is disconnected.
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SharedCfg, io::Io, testing::IoTest, time::Millis};

    #[crate::rt_test]
    async fn close_timeout() {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(4096);
        let cfg =
            SharedCfg::new("WS-TEST").add(ws::WsClientConfig::new().set_close_timeout(Millis(50)));
        let io = Io::new(server, cfg);
        let sink = WsSink::new(io.get_ref(), ws::Codec::new(), io.shared().get());

        sink.send(ws::Message::Close(None)).await.unwrap();
        assert!(!client.is_server_dropped());

        sleep(Millis(75)).await;
        assert!(sink.io().is_closed());
    }
}
