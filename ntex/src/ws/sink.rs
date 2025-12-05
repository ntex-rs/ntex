use std::rc::Rc;

use crate::{io::IoRef, io::OnDisconnect, ws};

#[derive(Clone, Debug)]
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

    /// Io reference
    pub fn io(&self) -> &IoRef {
        &self.0.io
    }

    /// Endcode and send message to the peer.
    pub async fn send(&self, item: ws::Message) -> Result<(), ws::error::ProtocolError> {
        let close = match item {
            ws::Message::Close(_) => self.0.codec.is_closed(),
            _ => false,
        };

        self.0.io.encode(item, &self.0.codec)?;
        if close {
            self.0.io.close();
        }
        Ok(())
    }

    /// Notify when connection get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}
