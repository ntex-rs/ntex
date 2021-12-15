use std::{future::Future, rc::Rc};

use crate::io::{Io, IoRef, OnDisconnect};
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
            inner.io.write().encode(item, &inner.codec)?;
            Ok(())
        }
    }

    /// Notify when connection get disconnected
    pub fn on_disconnect(&self) -> OnDisconnect {
        self.0.io.on_disconnect()
    }
}
