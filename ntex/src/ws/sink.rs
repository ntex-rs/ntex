use std::{future::Future, rc::Rc};

use crate::io::{IoRef, OnDisconnect};
use crate::ws;

pub struct WsSink(Rc<WsSinkInner>);

struct WsSinkInner {
    io: IoRef,
    codec: ws::Codec,
}

impl WsSink {
    pub(crate) fn new(io: IoRef, codec: ws::Codec) -> Self {
        Self(Rc::new(WsSinkInner { io, codec }))
    }

    /// Endcode and send message to the peer.
    pub fn send(
        &self,
        item: ws::Message,
    ) -> impl Future<Output = Result<(), ws::ProtocolError>> {
        let inner = self.0.clone();

        async move {
            let close = match item {
                ws::Message::Close(_) => inner.codec.is_closed(),
                _ => false,
            };

            let wrt = inner.io.write();
            wrt.write_ready(false).await?;
            wrt.encode(item, &inner.codec)?;
            if close {
                inner.io.close();
            }
            Ok(())
        }
    }

    /// Notify when connection get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}
