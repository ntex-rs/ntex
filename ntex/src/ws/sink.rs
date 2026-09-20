#![allow(clippy::unused_async)]
use std::rc::Rc;

use crate::{io::IoRef, io::OnDisconnect, ws};

#[derive(Clone, Debug)]
/// A clonable handle for sending messages over a WebSocket connection.
pub struct WsSink(Rc<WsSinkInner>);

#[derive(Debug)]
struct WsSinkInner {
    io: IoRef,
    codec: ws::Codec,
}

impl WsSink {
    pub(crate) fn new(io: IoRef, codec: ws::Codec) -> Self {
        Self(Rc::new(WsSinkInner { io, codec }))
    }

    /// Returns the underlying I/O handle.
    pub fn io(&self) -> &IoRef {
        &self.0.io
    }

    /// Encodes and queues a message for the peer.
    pub async fn send(&self, item: ws::Message) -> Result<(), ws::error::ProtocolError> {
        let close = matches!(item, ws::Message::Close(_));

        if let Err(e) = self.0.io.encode(item, &self.0.codec) {
            Err(e)
        } else {
            if close {
                self.0.io.close();
            }
            Ok(())
        }
    }

    /// Returns a future that resolves when the connection is disconnected.
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}
