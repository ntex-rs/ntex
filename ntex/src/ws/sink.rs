use std::{future::Future, rc::Rc};

use crate::{io::IoRef, io::OnDisconnect, util::Ready, ws};

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

    /// Endcode and send message to the peer
    pub fn send(
        &self,
        item: ws::Message,
    ) -> impl Future<Output = Result<(), ws::error::ProtocolError>> {
        let close = match item {
            ws::Message::Close(_) => self.0.codec.is_closed(),
            _ => false,
        };

        if let Err(e) = self.0.io.encode(item, &self.0.codec) {
            Ready::Err(e)
        } else {
            if close {
                self.0.io.close();
            }
            Ready::Ok(())
        }
    }

    /// Notify when connection get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}
